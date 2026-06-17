//! zagentmesh host daemon library.
//!
//! The daemon owns OS PTYs and agent processes and serves the control protocol
//! over a local Unix socket (and, in Phase 2, NetBird TCP). This crate exposes
//! the daemon's runtime so the `zagentmeshd` binary and integration tests can
//! drive it.
//!
//! Current scope (see `docs/plan-phase-1.md` "Build Order"): bind the Unix
//! socket with correct permissions, single-instance lock, stale-socket recovery,
//! `daemon.health`, PTY-backed local sessions, and raw attach streaming over a
//! separate connection. Agents, detection (the state engine), the SQLite store,
//! and the event log are later milestones and exist here only as stubs.

#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
// Unsafe is denied by default; the few FFI sites (advisory flock, socket chmod)
// opt back in with a localized `#[allow(unsafe_code)]` and a SAFETY comment.
#![deny(unsafe_code)]

pub mod error;
pub mod lock;
pub mod logging;
pub mod paths;

pub mod api;

// PTY ownership, the session registry/supervisor, and raw attach streaming are
// implemented; the remaining modules are future-milestone stubs (see plan
// "Cargo Workspace Layout").
pub mod pty;
pub mod session;

pub mod agent;
pub mod detect;
pub mod events;
pub mod store;

pub use error::DaemonError;
pub use paths::Paths;

/// Daemon build version (from Cargo). Reported by `daemon.health`.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
