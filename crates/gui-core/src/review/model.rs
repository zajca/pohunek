//! Review draft, comment, and lifecycle-status types.

// Rust guideline compliant 2026-10-01

use std::sync::atomic::{AtomicU64, Ordering};

use protocol::SessionId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::HostId;

/// Monotonic in-process counter mixed into freshly minted review ids.
static REVIEW_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stable identifier for one review draft; also its JSON store filename stem.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewId(String);

impl ReviewId {
    /// Borrow the review id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReviewId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Mints a fresh review id: `review-<unix-nanos>-<in-process-counter>`.
///
/// The workspace has no `uuid`/`rand` dependency
/// (`.agents/rust-guidelines/11_universal_guidelines.md` M-SMALLER-CRATES
/// discourages pulling one in for a single call site). A nanosecond
/// wall-clock reading mixed with a monotonic in-process counter is enough
/// entropy for a single desktop GUI process minting reviews one at a time
/// from operator input; the counter alone guarantees distinctness for two
/// reviews minted within the same nanosecond.
#[must_use]
pub fn new_review_id() -> ReviewId {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = REVIEW_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    ReviewId(format!("review-{nanos:020}-{counter}"))
}

/// What a review's diff was fetched from.
///
/// Both a session's own detail pane and its project's worktree list
/// (`docs/design/track-d-ui-brief.md` §3.9) resolve to the exact same
/// `session.diff` call and worktree, so this model uses one `Session`
/// variant for both UI entry points rather than duplicating a distinction
/// with no behavioral difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSource {
    /// A live session's worktree diff against its resolved base.
    Session {
        host_id: HostId,
        session_id: SessionId,
    },
    /// A GitHub pull request diff, fetched independently of any session.
    PullRequest { host_id: HostId, pr_number: u64 },
}

/// Which side of the diff a comment is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSide {
    /// The pre-image (old) side of the diff.
    Old,
    /// The post-image (new) side of the diff.
    New,
}

impl ReviewSide {
    /// Returns the stable label used in rendered comment blocks.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

/// One inline comment anchored to a `path:line` on one side of the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub path: String,
    pub side: ReviewSide,
    pub line: u32,
    pub text: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

impl ReviewComment {
    /// Creates a comment stamped with the current time.
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        side: ReviewSide,
        line: u32,
        text: impl Into<String>,
    ) -> Self {
        Self::new_at(path, side, line, text, OffsetDateTime::now_utc())
    }

    /// Creates a comment stamped with `now` as its creation time.
    pub(crate) fn new_at(
        path: impl Into<String>,
        side: ReviewSide,
        line: u32,
        text: impl Into<String>,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            path: path.into(),
            side,
            line,
            text: text.into(),
            created_at: format_rfc3339(now),
        }
    }
}

/// Lifecycle status of a review draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    /// Collecting comments; not yet dispatched.
    Draft,
    /// Dispatched as a new session; see [`Review::dispatched_session_id`].
    Dispatched,
}

/// A review draft: its source, collected comments, and dispatch lifecycle.
///
/// Persisted as one JSON file per review under the reviews store directory
/// (see [`crate::ReviewStore`]). The diff content itself is not stored here —
/// it is re-fetched and re-parsed on demand; this type only holds what the
/// operator added on top of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: ReviewId,
    pub source: ReviewSource,
    pub project: String,
    pub branch: String,
    #[serde(default)]
    pub comments: Vec<ReviewComment>,
    pub status: ReviewStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_session_id: Option<SessionId>,
    pub created_at: String,
    pub updated_at: String,
}

impl Review {
    /// Creates a new draft review with no comments.
    #[must_use]
    pub fn new(
        source: ReviewSource,
        project: impl Into<String>,
        branch: impl Into<String>,
    ) -> Self {
        Self::new_at(source, project, branch, OffsetDateTime::now_utc())
    }

    /// Creates a new draft review with no comments, created and last updated
    /// at `now`.
    pub(crate) fn new_at(
        source: ReviewSource,
        project: impl Into<String>,
        branch: impl Into<String>,
        now: OffsetDateTime,
    ) -> Self {
        let stamp = format_rfc3339(now);
        Self {
            id: new_review_id(),
            source,
            project: project.into(),
            branch: branch.into(),
            comments: Vec::new(),
            status: ReviewStatus::Draft,
            dispatched_session_id: None,
            created_at: stamp.clone(),
            updated_at: stamp,
        }
    }

    /// Appends a comment and bumps `updated_at`.
    pub fn add_comment(&mut self, comment: ReviewComment) {
        self.add_comment_at(comment, OffsetDateTime::now_utc());
    }

    /// Appends a comment and sets `updated_at` to `now`.
    pub(crate) fn add_comment_at(&mut self, comment: ReviewComment, now: OffsetDateTime) {
        self.comments.push(comment);
        self.updated_at = format_rfc3339(now);
    }

    /// Removes the comment at `index`, if present, and bumps `updated_at`.
    pub fn remove_comment(&mut self, index: usize) -> Option<ReviewComment> {
        self.remove_comment_at(index, OffsetDateTime::now_utc())
    }

    /// Removes the comment at `index`, if present, and sets `updated_at` to
    /// `now`. An out-of-range `index` leaves the review untouched.
    pub(crate) fn remove_comment_at(
        &mut self,
        index: usize,
        now: OffsetDateTime,
    ) -> Option<ReviewComment> {
        if index >= self.comments.len() {
            return None;
        }
        let removed = self.comments.remove(index);
        self.updated_at = format_rfc3339(now);
        Some(removed)
    }

    /// Replaces the text of the comment at `index` in place and bumps
    /// `updated_at`. Returns `false` when `index` is out of range (no-op).
    ///
    /// Added alongside [`Self::add_comment`]/[`Self::remove_comment`] for the
    /// GUI's inline comment editor (`docs/design/track-d-ui-brief.md` §3.9),
    /// which needs to edit an existing comment's text without disturbing its
    /// anchor (`path`/`side`/`line`) or `created_at`.
    pub fn edit_comment(&mut self, index: usize, text: impl Into<String>) -> bool {
        self.edit_comment_at(index, text, OffsetDateTime::now_utc())
    }

    /// Replaces the text of the comment at `index` in place and sets
    /// `updated_at` to `now`. Returns `false` when `index` is out of range
    /// (no-op).
    pub(crate) fn edit_comment_at(
        &mut self,
        index: usize,
        text: impl Into<String>,
        now: OffsetDateTime,
    ) -> bool {
        let Some(comment) = self.comments.get_mut(index) else {
            return false;
        };
        comment.text = text.into();
        self.updated_at = format_rfc3339(now);
        true
    }

    /// Marks this review dispatched to a newly created session.
    pub(crate) fn mark_dispatched(&mut self, session_id: SessionId) {
        self.mark_dispatched_at(session_id, OffsetDateTime::now_utc());
    }

    /// Marks this review dispatched to a newly created session and sets
    /// `updated_at` to `now`.
    pub(crate) fn mark_dispatched_at(&mut self, session_id: SessionId, now: OffsetDateTime) {
        self.status = ReviewStatus::Dispatched;
        self.dispatched_session_id = Some(session_id);
        self.updated_at = format_rfc3339(now);
    }
}

/// Formats `instant` as an RFC3339 string, matching the daemon's
/// notification store's timestamp convention
/// (`crates/daemon/src/notifications/mod.rs::timestamp_now`).
pub(crate) fn format_rfc3339(instant: OffsetDateTime) -> String {
    instant
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        format_rfc3339, new_review_id, Review, ReviewComment, ReviewSide, ReviewSource,
        ReviewStatus,
    };
    use crate::HostId;
    use protocol::SessionId;
    use time::macros::datetime;

    fn session_source() -> ReviewSource {
        ReviewSource::Session {
            host_id: HostId::new("host-1"),
            session_id: SessionId("s-1".to_owned()),
        }
    }

    fn pull_request_source() -> ReviewSource {
        ReviewSource::PullRequest {
            host_id: HostId::new("host-1"),
            pr_number: 42,
        }
    }

    #[test]
    fn new_review_ids_are_distinct_even_when_minted_back_to_back() {
        let first = new_review_id();
        let second = new_review_id();
        assert_ne!(first, second);
    }

    #[test]
    fn format_rfc3339_keeps_the_stored_timestamp_format() {
        assert_eq!(
            format_rfc3339(datetime!(2026-10-01 12:30:45 UTC)),
            "2026-10-01T12:30:45Z"
        );
        assert_eq!(
            format_rfc3339(datetime!(2026-10-01 12:30:45.5 UTC)),
            "2026-10-01T12:30:45.5Z"
        );
    }

    #[test]
    fn new_review_starts_as_a_draft_with_no_comments() {
        let created = datetime!(2026-10-01 08:00:00 UTC);
        let review = Review::new_at(session_source(), "project-1", "feature/x", created);

        assert_eq!(review.status, ReviewStatus::Draft);
        assert!(review.comments.is_empty());
        assert!(review.dispatched_session_id.is_none());
        assert_eq!(review.created_at, "2026-10-01T08:00:00Z");
        assert_eq!(review.updated_at, "2026-10-01T08:00:00Z");
    }

    #[test]
    fn comment_new_at_stamps_the_given_instant() {
        let comment = ReviewComment::new_at(
            "src/lib.rs",
            ReviewSide::New,
            10,
            "fix this",
            datetime!(2026-10-01 08:05:00 UTC),
        );

        assert_eq!(comment.created_at, "2026-10-01T08:05:00Z");
    }

    #[test]
    fn real_clock_wrappers_stamp_parseable_rfc3339_timestamps() {
        let mut review = Review::new(pull_request_source(), "project-1", "feature/x");
        review.add_comment(ReviewComment::new("f.rs", ReviewSide::Old, 3, "why"));

        for stamp in [
            &review.created_at,
            &review.updated_at,
            &review.comments[0].created_at,
        ] {
            assert!(
                stamp.ends_with('Z') && stamp.contains('T'),
                "not an RFC 3339 UTC timestamp: {stamp}"
            );
        }
    }

    #[test]
    fn add_and_remove_comment_round_trip() {
        let mut review = Review::new_at(
            pull_request_source(),
            "project-1",
            "feature/x",
            datetime!(2026-10-01 08:00:00 UTC),
        );
        review.add_comment_at(
            ReviewComment::new_at(
                "src/lib.rs",
                ReviewSide::New,
                10,
                "fix this",
                datetime!(2026-10-01 08:01:00 UTC),
            ),
            datetime!(2026-10-01 08:01:00 UTC),
        );
        assert_eq!(review.comments.len(), 1);
        assert_eq!(review.updated_at, "2026-10-01T08:01:00Z");

        let removed = review
            .remove_comment_at(0, datetime!(2026-10-01 08:02:00 UTC))
            .expect("comment removed");
        assert_eq!(removed.path, "src/lib.rs");
        assert!(review.comments.is_empty());
        assert_eq!(review.updated_at, "2026-10-01T08:02:00Z");

        assert!(review
            .remove_comment_at(0, datetime!(2026-10-01 08:03:00 UTC))
            .is_none());
        assert_eq!(review.updated_at, "2026-10-01T08:02:00Z");
    }

    #[test]
    fn edit_comment_replaces_text_in_place_and_sets_updated_at_to_the_given_instant() {
        let created = datetime!(2026-10-01 08:00:00 UTC);
        let edited_at = datetime!(2026-10-01 08:10:00 UTC);
        let mut review = Review::new_at(session_source(), "project-1", "feature/x", created);
        review.add_comment_at(
            ReviewComment::new_at("src/lib.rs", ReviewSide::New, 10, "typo", created),
            created,
        );

        let edited = review.edit_comment_at(0, "fix the typo instead", edited_at);

        assert!(edited);
        assert_eq!(review.comments[0].text, "fix the typo instead");
        assert_eq!(review.comments[0].created_at, "2026-10-01T08:00:00Z");
        assert_eq!(review.updated_at, "2026-10-01T08:10:00Z");
    }

    #[test]
    fn edits_at_the_same_instant_and_at_a_later_instant_are_recorded_exactly() {
        // A coarse or non-monotonic wall clock can hand two consecutive
        // operations the same or an earlier reading; the model records
        // whatever instant it is given, so ordering never depends on it.
        let instant = datetime!(2026-10-01 08:00:00 UTC);
        let later = datetime!(2026-10-01 08:00:01 UTC);
        let mut review = Review::new_at(session_source(), "project-1", "feature/x", instant);
        review.add_comment_at(
            ReviewComment::new_at("src/lib.rs", ReviewSide::New, 10, "typo", instant),
            instant,
        );
        assert_eq!(review.updated_at, review.created_at);

        assert!(review.edit_comment_at(0, "first edit", instant));
        assert_eq!(review.updated_at, "2026-10-01T08:00:00Z");
        assert_eq!(review.updated_at, review.created_at);

        assert!(review.edit_comment_at(0, "second edit", later));
        assert_eq!(review.updated_at, "2026-10-01T08:00:01Z");
        assert_ne!(review.updated_at, review.created_at);
        assert_eq!(review.comments[0].text, "second edit");
    }

    #[test]
    fn edit_comment_out_of_range_returns_false_without_touching_the_review() {
        let created = datetime!(2026-10-01 08:00:00 UTC);
        let mut review = Review::new_at(pull_request_source(), "project-1", "feature/x", created);

        assert!(!review.edit_comment_at(
            0,
            "no comment at this index",
            datetime!(2026-10-01 09:00:00 UTC)
        ));
        assert_eq!(review.updated_at, "2026-10-01T08:00:00Z");
    }

    #[test]
    fn mark_dispatched_sets_status_session_id_and_updated_at() {
        let mut review = Review::new_at(
            session_source(),
            "project-1",
            "feature/x",
            datetime!(2026-10-01 08:00:00 UTC),
        );

        review.mark_dispatched_at(
            SessionId("s-2".to_owned()),
            datetime!(2026-10-01 08:20:00 UTC),
        );

        assert_eq!(review.status, ReviewStatus::Dispatched);
        assert_eq!(
            review.dispatched_session_id,
            Some(SessionId("s-2".to_owned()))
        );
        assert_eq!(review.created_at, "2026-10-01T08:00:00Z");
        assert_eq!(review.updated_at, "2026-10-01T08:20:00Z");
    }

    #[test]
    fn review_json_round_trips_through_serde() {
        let instant = datetime!(2026-10-01 08:00:00 UTC);
        let mut review = Review::new_at(session_source(), "project-1", "feature/x", instant);
        review.add_comment_at(
            ReviewComment::new_at("f.rs", ReviewSide::Old, 3, "why", instant),
            instant,
        );

        let json = serde_json::to_string(&review).expect("serialize review");
        let parsed: Review = serde_json::from_str(&json).expect("deserialize review");

        assert_eq!(parsed, review);
    }
}
