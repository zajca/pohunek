//! Projects: automatic git-repo awareness for sessions.
//!
//! A **project** is a lightweight record of a git repository the daemon has seen
//! on this host — keyed by the canonical `git_common_dir`, so the main checkout
//! and every linked worktree of one repo collapse to a single logical project
//! (see `docs/design/projects.md`). Projects accrue as a side effect of working:
//! starting a session inside a work tree auto-registers one; `pohunek project
//! add` registers one explicitly. There is deliberately no filesystem scan.
//!
//! This module owns the pure detection unit ([`detect`]) and the project id
//! derivation ([`detect::project_id`]), and [`ProjectManager`] on top of them:
//! the store glue (auto-registration + manual upsert) and `<id|label>` reference
//! resolution.

pub mod detect;

use std::io;
use std::path::Path;
use std::sync::Arc;

use protocol::{ErrorClass, ProjectSource, ProtocolError};

use crate::store::{ProjectRecord, ProjectResolution, Store};

use detect::DetectedProject;

/// Project store glue: resolve `<id|label>` references and upsert detected/added
/// projects into the unified [`Store`].
///
/// Holds the shared [`Store`] (`Arc`) — the same instance the session registry
/// and worktree manager write through, so all record kinds stay behind one
/// serialization point. Detection itself ([`detect`]) is pure and lives beside
/// this; the manager only adds the policy (what to preserve on re-registration,
/// how to map a missing/ambiguous reference to a typed error).
#[derive(Debug)]
pub struct ProjectManager {
    store: Arc<Store>,
}

impl ProjectManager {
    /// Build a manager over the shared metadata store.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// The shared metadata store, for the `project.*` handlers and tests.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Resolve a `<id|label>` reference to a project, mapping a miss or a
    /// collision to a typed [`ProtocolError`] the CLI renders directly
    /// (design Decision 2).
    pub fn resolve(&self, reference: &str) -> Result<ProjectRecord, ProtocolError> {
        match self.store.find_project(reference).map_err(store_error)? {
            ProjectResolution::Found(record) => Ok(record),
            ProjectResolution::NotFound => Err(project_not_found(reference)),
            ProjectResolution::Ambiguous(candidates) => {
                Err(project_ambiguous(reference, &candidates))
            }
        }
    }

    /// Upsert a project from a [`DetectedProject`], returning the stored record.
    ///
    /// Re-registration **preserves operator-owned fields** (`custom_name`,
    /// `default_base_branch`, `source`, `added_at`) and only refreshes what
    /// detection actually observes (`repo_root`, `origin_url`, `is_bare`) plus
    /// `last_used_at`. A brand-new project is recorded as [`ProjectSource::Auto`]
    /// when `manual` is false, else [`ProjectSource::Manual`]; re-registering an
    /// existing `Auto` project as manual promotes it (so it is never treated as
    /// stale auto data), but auto-registration never demotes a `Manual` project.
    pub fn register(
        &self,
        detected: &DetectedProject,
        now: &str,
        manual: bool,
    ) -> Result<ProjectRecord, ProtocolError> {
        let existing = self
            .store
            .load_projects()
            .map_err(store_error)?
            .into_iter()
            .find(|project| project.git_common_dir == detected.git_common_dir);

        let record = match existing {
            Some(mut prev) => {
                prev.repo_root = detected.repo_root.clone();
                prev.origin_url = detected.origin_url.clone();
                prev.is_bare = detected.is_bare;
                prev.last_used_at = now.to_owned();
                // A manual (re-)add promotes an auto record; auto-registration
                // never demotes a manual one.
                if manual {
                    prev.source = ProjectSource::Manual;
                }
                prev
            }
            None => ProjectRecord {
                git_common_dir: detected.git_common_dir.clone(),
                repo_root: detected.repo_root.clone(),
                custom_name: None,
                origin_url: detected.origin_url.clone(),
                default_base_branch: None,
                is_bare: detected.is_bare,
                source: if manual {
                    ProjectSource::Manual
                } else {
                    ProjectSource::Auto
                },
                added_at: now.to_owned(),
                last_used_at: now.to_owned(),
            },
        };
        self.store.record_project(&record).map_err(store_error)?;
        Ok(record)
    }

    /// Bump a project's `last_used_at` to `now` (a session started in it). Keyed
    /// by the canonical `git_common_dir` and re-read from the store before writing
    /// so a concurrent rename/add is not clobbered. Returns the updated record, or
    /// `None` if the project no longer exists (a benign race with `project rm`).
    /// The data model defines `last_used_at` as bumped on each session start, so
    /// the `--project` reference path must do this too — not only auto-detection.
    pub fn touch(
        &self,
        git_common_dir: &Path,
        now: &str,
    ) -> Result<Option<ProjectRecord>, ProtocolError> {
        let existing = self
            .store
            .load_projects()
            .map_err(store_error)?
            .into_iter()
            .find(|project| project.git_common_dir == git_common_dir);
        let Some(mut record) = existing else {
            return Ok(None);
        };
        record.last_used_at = now.to_owned();
        self.store.record_project(&record).map_err(store_error)?;
        Ok(Some(record))
    }
}

/// Map a project-store I/O failure to a typed runtime error.
fn store_error(err: io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "project_store_error",
        format!("project store I/O failed: {err}"),
        None,
    )
}

/// No project matched a `<id|label>` reference.
fn project_not_found(reference: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "project_not_found",
        format!("no project matches {reference:?}"),
        Some(
            "run `pohunek project list` to see known projects, or `project add` to register one"
                .to_owned(),
        ),
    )
}

/// A `<label>` reference matched several projects; list the candidate ids and
/// paths so the operator can disambiguate with an id (design Decision 2).
fn project_ambiguous(reference: &str, candidates: &[ProjectRecord]) -> ProtocolError {
    let list = candidates
        .iter()
        .map(|candidate| format!("{} ({})", candidate.id(), candidate.repo_root.display()))
        .collect::<Vec<_>>()
        .join(", ");
    ProtocolError::new(
        ErrorClass::Runtime,
        "project_ambiguous",
        format!("{reference:?} matches several projects: {list}"),
        Some("disambiguate with the project id (the `p-…` value shown above)".to_owned()),
    )
}

/// Detect the project rooted at `path`, mapping a genuine I/O fault to a typed
/// error. A non-git directory (or any non-fatal detection failure) is `Ok(None)`.
pub fn detect_at(path: &Path) -> Result<Option<DetectedProject>, ProtocolError> {
    detect::detect(path).map_err(|err| {
        ProtocolError::new(
            ErrorClass::Runtime,
            "project_detect_failed",
            format!("git detection failed: {err}"),
            None,
        )
    })
}
