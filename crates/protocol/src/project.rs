//! Typed payloads for the `project.*` method family.
//!
//! A **project** is a git repository the daemon has seen on its host, keyed
//! internally by the canonical `git_common_dir` (see `docs/design/projects.md`).
//! These shared types define the JSON shapes the CLI and daemon exchange inside
//! the generic request/response envelopes for project lifecycle methods, mirroring
//! the role [`crate::SessionInfo`] plays for sessions.

use serde::{Deserialize, Serialize};

/// How a project entered the store.
///
/// `Auto` records accrue as a side effect of starting sessions inside a work
/// tree; `Manual` records are added explicitly via `pohunek project add`.
/// Re-adding an `Auto` project flips it to `Manual` so it is never treated as
/// stale auto data (if garbage collection is ever added).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    /// Auto-registered when a session started inside the repository's work tree.
    Auto,
    /// Added (or re-added) explicitly by the operator.
    Manual,
}
