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
//! derivation ([`detect::project_id`]); the store glue and reference resolution
//! (`ProjectManager`) build on top of it in later milestones.

pub mod detect;
