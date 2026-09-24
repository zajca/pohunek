//! Documented values the installer writes and the bounds it waits within.
//!
//! Every value written into `service.toml` is an explicit constant here, so
//! the file never depends on a serde default and each number carries its
//! rationale in one place.

// Rust guideline compliant 2026-09-24

use std::time::Duration;

/// How long the daemon waits for a new worker's private socket.
///
/// Ten seconds covers a cold start of `pohunek-sessiond` under a loaded
/// service manager. Lower values turn slow registrations into spurious
/// post-timeout reconciliations; the engine never retries blindly either way.
pub const WORKER_CONNECT: Duration = Duration::from_secs(10);

/// How long a worker waits for `Initialize` before recording `never_initialized`.
///
/// Mirrors the `TimeoutStartSec=45s` the durable-worker RFC settled for the
/// systemd template, so both backends expire an uninitialized worker alike.
pub const WORKER_INITIALIZE: Duration = Duration::from_secs(45);

/// Deadline of one `/bin/launchctl` command.
///
/// `bootstrap` and `print` finish in milliseconds; ten seconds only guards
/// against a wedged launchd. `bootout` gets the job's exit timeout on top.
pub const LAUNCHCTL_COMMAND: Duration = Duration::from_secs(10);

/// `SIGTERM`-to-`SIGKILL` grace of one worker job.
///
/// Matches `TimeoutStopSec=30s` of the durable-worker RFC: long enough for an
/// agent to flush its transcript after the PTY hangup.
pub const WORKER_EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// `SIGTERM`-to-`SIGKILL` grace of the daemon job.
///
/// The daemon persists its registry on shutdown; thirty seconds matches the
/// worker grace so neither side is cut short during a coordinated stop.
pub const DAEMON_EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum delay before the service manager restarts a failed daemon.
///
/// Mirrors `RestartSec=5s` of the historical unit and launchd's
/// `ThrottleInterval`; shorter values let a crash loop flood the logs.
pub const DAEMON_RESTART_THROTTLE: Duration = Duration::from_secs(5);

/// Grace between `SIGTERM` and `SIGKILL` in the lost-generation marker sweep.
///
/// Five seconds lets a hangup-ignoring agent exit on `SIGTERM` without
/// keeping a crashed session's processes around for long.
pub const SWEEP_GRACE: Duration = Duration::from_secs(5);

/// Open-file soft and hard limit of daemon and worker jobs.
///
/// launchd defaults to 256 descriptors, which a daemon holding many worker
/// sockets and event logs exhausts; 8192 matches common Linux user defaults.
pub const OPEN_FILES: u64 = 8_192;

/// How long the service manager waits for the daemon to report readiness.
///
/// Startup reconciles every durable session before `READY=1`, which can take
/// tens of seconds on a host with many sessions.
pub const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `pohunek service` waits for the daemon to answer `daemon.health`.
///
/// The start timeout plus a margin for the restart throttle, so the
/// installer never gives up while the service manager is still trying.
pub const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(75);

/// Interval between readiness and settlement polls.
///
/// A quarter second keeps the wait responsive without busy-polling the
/// daemon socket or the service manager.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Bound on one service-manager call (D-Bus on Linux).
///
/// Matches the launchd command deadline; a healthy manager answers in
/// milliseconds.
pub const SUPERVISOR_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on one control request to the local daemon.
///
/// `session.stop` returns once the worker accepted the stop, so ten seconds
/// covers a loaded daemon without hiding a hung one.
pub const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long uninstall waits for stopped sessions to settle.
///
/// Three worker exit timeouts: the agent's grace, the worker's own output
/// drain, and the daemon acknowledging the terminal state.
pub const SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(90);

/// Deadline for one staged binary's `--version` probe.
///
/// Printing a version needs no I/O; five seconds only guards against a
/// binary that ignores `--version` and starts running.
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bytes of `--version` output kept from a staged binary.
///
/// A version line is tens of bytes; the cap bounds memory for a binary that
/// floods stdout.
pub const VERSION_PROBE_OUTPUT: usize = 4 * 1024;

/// Deadline for `systemd-analyze verify` on the rendered units.
///
/// Verification loads the unit and its dependencies from disk; thirty seconds
/// tolerates a slow first run without hanging the installer.
pub const UNIT_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes of `systemd-analyze` diagnostics kept for an error message.
///
/// Enough for a screenful of warnings while keeping errors bounded.
pub const UNIT_VERIFY_OUTPUT: usize = 16 * 1024;

/// Longest wait for the install transaction lock.
///
/// Long enough to ride out a concurrent `pohunek service status`, which holds
/// the lock only while probing it; far shorter than any real transaction, so
/// a second install, upgrade, or uninstall is refused promptly.
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Interval between attempts to take a contended transaction lock.
///
/// A status probe releases the lock within microseconds; polling every 50 ms
/// keeps the wait responsive without spinning.
pub const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Maximum size of the install transaction record.
///
/// The record holds a few short fields; anything larger is corrupt.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
