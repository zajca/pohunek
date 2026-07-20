//! Review draft, comment, and lifecycle-status types.

// Rust guideline compliant 2026-07-20

use std::sync::atomic::{AtomicU64, Ordering};

use protocol::SessionId;
use serde::{Deserialize, Serialize};

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
        Self {
            path: path.into(),
            side,
            line,
            text: text.into(),
            created_at: now_rfc3339(),
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
        let now = now_rfc3339();
        Self {
            id: new_review_id(),
            source,
            project: project.into(),
            branch: branch.into(),
            comments: Vec::new(),
            status: ReviewStatus::Draft,
            dispatched_session_id: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Appends a comment and bumps `updated_at`.
    pub fn add_comment(&mut self, comment: ReviewComment) {
        self.comments.push(comment);
        self.updated_at = now_rfc3339();
    }

    /// Removes the comment at `index`, if present, and bumps `updated_at`.
    pub fn remove_comment(&mut self, index: usize) -> Option<ReviewComment> {
        if index >= self.comments.len() {
            return None;
        }
        let removed = self.comments.remove(index);
        self.updated_at = now_rfc3339();
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
        let Some(comment) = self.comments.get_mut(index) else {
            return false;
        };
        comment.text = text.into();
        self.updated_at = now_rfc3339();
        true
    }

    /// Marks this review dispatched to a newly created session.
    pub(crate) fn mark_dispatched(&mut self, session_id: SessionId) {
        self.status = ReviewStatus::Dispatched;
        self.dispatched_session_id = Some(session_id);
        self.updated_at = now_rfc3339();
    }
}

/// Returns the current time as an RFC3339 string, matching the daemon's
/// notification store's timestamp convention
/// (`crates/daemon/src/notifications/mod.rs::timestamp_now`).
pub(crate) fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{new_review_id, Review, ReviewComment, ReviewSide, ReviewSource, ReviewStatus};
    use crate::HostId;
    use protocol::SessionId;

    #[test]
    fn new_review_ids_are_distinct_even_when_minted_back_to_back() {
        let first = new_review_id();
        let second = new_review_id();
        assert_ne!(first, second);
    }

    #[test]
    fn new_review_starts_as_a_draft_with_no_comments() {
        let review = Review::new(
            ReviewSource::Session {
                host_id: HostId::new("host-1"),
                session_id: SessionId("s-1".to_owned()),
            },
            "project-1",
            "feature/x",
        );

        assert_eq!(review.status, ReviewStatus::Draft);
        assert!(review.comments.is_empty());
        assert!(review.dispatched_session_id.is_none());
        assert_eq!(review.created_at, review.updated_at);
    }

    #[test]
    fn add_and_remove_comment_round_trip() {
        let mut review = Review::new(
            ReviewSource::PullRequest {
                host_id: HostId::new("host-1"),
                pr_number: 42,
            },
            "project-1",
            "feature/x",
        );
        review.add_comment(ReviewComment::new(
            "src/lib.rs",
            ReviewSide::New,
            10,
            "fix this",
        ));
        assert_eq!(review.comments.len(), 1);

        let removed = review.remove_comment(0).expect("comment removed");
        assert_eq!(removed.path, "src/lib.rs");
        assert!(review.comments.is_empty());
        assert!(review.remove_comment(0).is_none());
    }

    #[test]
    fn edit_comment_replaces_text_in_place_and_bumps_updated_at() {
        let mut review = Review::new(
            ReviewSource::Session {
                host_id: HostId::new("host-1"),
                session_id: SessionId("s-1".to_owned()),
            },
            "project-1",
            "feature/x",
        );
        review.add_comment(ReviewComment::new(
            "src/lib.rs",
            ReviewSide::New,
            10,
            "typo",
        ));
        let created_at = review.comments[0].created_at.clone();
        let updated_at_before_edit = review.updated_at.clone();

        let edited = review.edit_comment(0, "fix the typo instead");

        assert!(edited);
        assert_eq!(review.comments[0].text, "fix the typo instead");
        assert_eq!(review.comments[0].created_at, created_at);
        assert!(review.updated_at >= updated_at_before_edit);
    }

    #[test]
    fn edit_comment_out_of_range_returns_false_without_touching_the_review() {
        let mut review = Review::new(
            ReviewSource::PullRequest {
                host_id: HostId::new("host-1"),
                pr_number: 1,
            },
            "project-1",
            "feature/x",
        );
        let updated_at_before = review.updated_at.clone();

        assert!(!review.edit_comment(0, "no comment at this index"));
        assert_eq!(review.updated_at, updated_at_before);
    }

    #[test]
    fn mark_dispatched_sets_status_and_session_id() {
        let mut review = Review::new(
            ReviewSource::Session {
                host_id: HostId::new("host-1"),
                session_id: SessionId("s-1".to_owned()),
            },
            "project-1",
            "feature/x",
        );

        review.mark_dispatched(SessionId("s-2".to_owned()));

        assert_eq!(review.status, ReviewStatus::Dispatched);
        assert_eq!(
            review.dispatched_session_id,
            Some(SessionId("s-2".to_owned()))
        );
    }

    #[test]
    fn review_json_round_trips_through_serde() {
        let mut review = Review::new(
            ReviewSource::Session {
                host_id: HostId::new("host-1"),
                session_id: SessionId("s-1".to_owned()),
            },
            "project-1",
            "feature/x",
        );
        review.add_comment(ReviewComment::new("f.rs", ReviewSide::Old, 3, "why"));

        let json = serde_json::to_string(&review).expect("serialize review");
        let parsed: Review = serde_json::from_str(&json).expect("deserialize review");

        assert_eq!(parsed, review);
    }
}
