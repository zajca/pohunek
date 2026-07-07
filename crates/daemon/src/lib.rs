//! pohunek host daemon library.
//!
//! The daemon owns OS PTYs and agent processes and serves the control protocol
//! over a local Unix socket and optional `NetBird` TCP. This crate exposes
//! the daemon's runtime so the `pohunekd` binary and integration tests can
//! drive it.
//!
//! Current scope: bind the Unix socket with correct permissions,
//! single-instance lock, stale-socket recovery, `daemon.health`, PTY-backed
//! local sessions, raw attach streaming over a separate connection, agents,
//! detection (the state engine), worktree-per-session binding, a unified
//! JSON-lines metadata store (resume + worktree bindings), an append-only event
//! log, and direct remote transport over `NetBird`.

// Unsafe is denied by default; the few FFI sites (advisory flock, socket chmod,
// pidfd syscalls) opt back in with localized `#[expect(unsafe_code)]` and SAFETY
// comments.
#![deny(unsafe_code)]

pub mod error;
pub mod lock;
pub mod logging;
pub mod paths;

pub mod api;
pub mod assistant;
pub mod capabilities;
pub mod discovery;
pub mod doctor;

pub mod pty;
pub mod session;

pub mod agent;
pub mod detect;
pub mod events;
mod external;
pub mod integration;
pub mod notifications;
pub mod procwatch;
pub mod project;
pub mod store;
pub(crate) mod time;
pub mod worktree;

pub use error::DaemonError;
pub use paths::Paths;

/// Daemon build version (from Cargo). Reported by `daemon.health`.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static XDG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
