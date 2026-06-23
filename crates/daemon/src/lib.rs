//! pohunek host daemon library.
//!
//! The daemon owns OS PTYs and agent processes and serves the control protocol
//! over a local Unix socket (and, in Phase 2, NetBird TCP). This crate exposes
//! the daemon's runtime so the `pohunekd` binary and integration tests can
//! drive it.
//!
//! Current scope (see `docs/plan-phase-1.md` "Build Order"): bind the Unix
//! socket with correct permissions, single-instance lock, stale-socket recovery,
//! `daemon.health`, PTY-backed local sessions, raw attach streaming over a
//! separate connection, agents, detection (the state engine), worktree-per-session
//! binding, a unified JSON-lines metadata store (resume + worktree bindings), and
//! an append-only event log. The SQLite store is deferred (see `NEXT.md`).

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
pub mod capabilities;
pub mod discovery;

// PTY ownership, the session registry/supervisor, and raw attach streaming are
// implemented; the remaining modules are future-milestone stubs (see plan
// "Cargo Workspace Layout").
pub mod pty;
pub mod session;

pub mod agent;
pub mod detect;
pub mod events;
pub mod integration;
pub mod project;
pub mod store;
pub mod worktree;

pub use error::DaemonError;
pub use paths::Paths;

/// Daemon build version (from Cargo). Reported by `daemon.health`.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
