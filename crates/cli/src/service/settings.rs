//! Documented values the installer writes and the bounds it waits within.
//!
//! Every value written into `service.toml` is an explicit constant here, so
//! the file never depends on a serde default and each number carries its
//! rationale in one place.

// Rust guideline compliant 2026-10-01

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

/// Time the systemd user manager may take to report its environment.
pub const MANAGER_ENVIRONMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes of the user manager's environment report that are kept.
pub const MANAGER_ENVIRONMENT_OUTPUT: usize = 1024 * 1024;

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

/// Bytes of the preflight report kept from the new daemon.
///
/// A session verdict is a few hundred bytes, so four MiB covers thousands of
/// live sessions; the cap bounds memory for a binary that floods stdout.
pub const PREFLIGHT_OUTPUT: usize = 4 * 1024 * 1024;

/// Bytes of the preflight's diagnostics kept for an error message.
///
/// Enough for the daemon's one-line failure while keeping errors bounded.
pub const PREFLIGHT_DIAGNOSTICS: usize = 4 * 1024;

/// Maximum metadata store size examined before restoring a legacy daemon.
///
/// Matches the daemon's durable store read limit; a larger store cannot be
/// proven readable by the previous release and must not be downgraded.
pub const ROLLBACK_STORE_BYTES: usize = 16 * 1024 * 1024;

/// Deadline for `pohunekd ensure-official-runtimes --dry-run`.
///
/// The plan reads the release catalog, the trust anchor, the store and the
/// profiles, and never writes; the thirty seconds only guards a wedged disk.
pub const RUNTIMES_PLAN_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for `pohunekd ensure-official-runtimes`, the apply.
///
/// The apply verifies and extracts each official package archive (at most
/// `MAX_COMPRESSED_BYTES` each) while no daemon runs, so a host with slow
/// storage needs minutes; a wedged apply still ends and rolls back.
pub const RUNTIMES_APPLY_TIMEOUT: Duration = Duration::from_secs(600);

/// Bytes of the runtime report kept from the daemon.
///
/// A report lists one entry per used runtime and profile, so one MiB covers
/// any realistic host; the cap bounds memory for a binary that floods stdout.
pub const RUNTIMES_OUTPUT: usize = 1024 * 1024;

/// Most unknown `service.toml` keys a warning names before it counts the rest.
///
/// A hand-edited file can hold thousands of unknown keys; the warning stays a
/// few lines, and the daemon log and `pohunek doctor` carry the full list.
pub const MAX_LISTED_UNKNOWN_KEYS: usize = 8;

/// Largest state file the runtime step backs up or restores.
///
/// The package registry grows with the installed packages and each profile is
/// a small TOML file; the daemon's own readers bound both far below this
/// ceiling, so a larger file is refused instead of copied.
pub const RUNTIME_STATE_BYTES: usize = 16 * 1024 * 1024;

/// Deadline for `systemd-analyze verify` on the rendered units.
///
/// Verification loads the unit and its dependencies from disk; thirty seconds
/// tolerates a slow first run without hanging the installer.
pub const UNIT_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes of `systemd-analyze` diagnostics kept for an error message.
///
/// Enough for a screenful of warnings while keeping errors bounded.
pub const UNIT_VERIFY_OUTPUT: usize = 16 * 1024;

/// Longest wait for `exec` of a file this process just wrote to stop failing
/// with `ETXTBSY`.
///
/// While any thread of the process holds a file open for writing, a sibling
/// thread's `fork` or `posix_spawn` gives its child a copy of that descriptor
/// until the child's own `exec`, and executing the file fails meanwhile. The
/// child execs within microseconds; the bound only absorbs a child that the
/// scheduler holds back on a loaded host, and expiry means the file stayed
/// open for writing.
pub const EXEC_BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Most symbolic links followed while classifying the release directory an
/// operator names, so a link loop is refused instead of followed forever.
pub const MAX_SOURCE_LINK_HOPS: usize = 40;

/// Interval between attempts to execute a file that is busy.
///
/// Short, because the busy window is the pre-`exec` gap of another child.
pub const EXEC_BUSY_POLL: std::time::Duration = std::time::Duration::from_millis(5);

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

/// Maximum size of the transaction lock holder record.
///
/// The record holds two numbers and a 64-character token; anything larger
/// is not a record `pohunek service lock` wrote.
pub const MAX_HOLDER_BYTES: usize = 4 * 1024;

/// Maximum size of the install transaction record.
///
/// The record holds a few short fields and the accepted-risk set the gate
/// journaled, which the preflight report (at most 4 MiB) bounds even when
/// pretty-printed; anything larger is corrupt. A record that would exceed it
/// is refused when it is saved, so every record that is written can be read.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Maximum size of the accepted-risk ledger.
///
/// The ledger holds one short entry per at-risk session of the last upgrade,
/// a subset of what the record journaled, so it is bounded by
/// [`MAX_RECORD_BYTES`]; anything larger is corrupt. A ledger that would
/// exceed it is refused when it is written.
pub const MAX_LEDGER_BYTES: usize = MAX_RECORD_BYTES;

pub use pohunek_platform::shell_env::{
    DEFAULT_LOGIN_SHELL, LOGIN_SHELL_OUTPUT, LOGIN_SHELL_TIMEOUT,
};
