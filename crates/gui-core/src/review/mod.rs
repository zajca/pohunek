//! Diff parsing, review drafts, the persistent review store, and
//! dispatch-to-session for Track D.6 (`docs/design/track-d-native-app.md` §6).
//!
//! Reviewing a change set happens in three headless steps, all testable
//! without any Iced dependency (the `gui` crate wires these into view state
//! and `Message`s):
//!
//! 1. Fetch diff text (`session.diff` via [`crate::diff_session`], or
//!    `gh pr diff` via [`crate::providers::github::GitHubClient`]) and parse
//!    it with [`parse_unified_diff`] into a [`DiffModel`]. Both sources
//!    produce the same unified-diff text, so one parser serves both.
//! 2. Collect comments into a [`Review`] draft and persist it with
//!    [`ReviewStore`] (one JSON file per review, atomic write-then-rename).
//! 3. Render the review prompt ([`render_review_prompt`]) and
//!    [`dispatch_review`] it as a new session in the reviewed worktree.

// Rust guideline compliant 2026-07-19

mod diff;
mod dispatch;
mod model;
mod store;

#[doc(inline)]
pub use diff::{
    parse_unified_diff, DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind, DiffModel,
};
#[doc(inline)]
pub use dispatch::{
    dispatch_review, render_review_prompt, ReviewDispatchParams, REVIEW_DISPATCHED_AT_KEY,
    REVIEW_SOURCE_KEY,
};
#[doc(inline)]
pub use model::{
    new_review_id, Review, ReviewComment, ReviewId, ReviewSide, ReviewSource, ReviewStatus,
};
#[doc(inline)]
pub use store::{default_reviews_dir, ReviewLoadError, ReviewStore, ReviewStoreError};
