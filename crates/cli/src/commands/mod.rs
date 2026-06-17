//! CLI command implementations.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. Each
//! command supports `--json` where the plan calls for machine-readable output
//! (see `docs/plan-phase-1.md` "CLI Grammar").

pub(crate) mod daemon;
pub(crate) mod doctor;
pub(crate) mod health;
pub(crate) mod session;
