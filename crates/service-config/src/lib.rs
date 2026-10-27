//! Loads, validates, and writes the installation's `service.toml`.
//!
//! `pohunek service install` records every value the native supervision path
//! needs in `<config>/pohunek/service.toml`. The daemon (`pohunekd
//! --service-config <abs>`) and each worker generation (`pohunek-sessiond
//! --service-config <abs>`) load the same file and fail fast when it is
//! missing or invalid. There are no defaults: every key of the current schema
//! is required, and each value is checked against a documented bound. Unknown
//! keys are tolerated outside the security tables (see
//! [Compatibility](#compatibility)).
//!
//! # Schema
//!
//! ```toml
//! schema_version = 7
//! prefix = "/home/u/.local"                       # absolute install prefix
//! active_version = "0.31.6"                       # <prefix>/libexec/pohunek/<version>
//!
//! [namespace]                                      # inputs of the job namespace
//! uid = 1000
//! state_root = "/home/u/.local/state/pohunek"     # canonical application state dir
//! runtime_root = "/run/user/1000/pohunek"         # canonical application runtime dir
//!
//! [deadlines]                                      # milliseconds, 1..=600000
//! worker_connect_ms = 10000
//! worker_initialize_ms = 45000
//! launchctl_command_ms = 10000
//! worker_exit_timeout_ms = 30000
//! daemon_exit_timeout_ms = 30000
//! daemon_restart_throttle_ms = 5000
//!
//! [environment]
//! search_path = ["/opt/homebrew/bin", "/usr/bin"]  # absolute normalized directories; may be empty
//! allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]   # names or trailing-`*` prefixes
//!
//! [sweep]
//! grace_ms = 5000                                  # 1..=600000
//!
//! [input]                                         # milliseconds, 1..=600000
//! initial_startup_grace_ms = 5000
//! submit_delay_ms = 150
//! write_stall_timeout_ms = 3000
//!
//! [waits]                                          # milliseconds, 5000..=3600000
//! turn_default_ms = 300000                         # at most turn_max_ms
//! turn_max_ms = 1800000
//! keepalive_idle_ms = 30000                        # optional; whole seconds, 1000..=600000
//! keepalive_interval_ms = 10000                    # optional; whole seconds, 1000..=600000
//! keepalive_count = 3                              # optional; 1..=10
//!
//! [hooks]
//! stderr_tail_bytes = 4096                         # 0..=32768; 0 disables the capture
//! required = ["post-create"]                       # optional; omitted while empty
//!
//! [events]                                        # bounds <data>/events/events.jsonl
//! max_file_bytes = 33554432                        # 1048576..=1073741824; one journal file
//! max_files = 8                                    # 2..=64; including the active file
//! max_age_days = 14                                # 1..=3650
//!
//! [inventory]                                      # optional table; every key optional
//! probe_cache_ttl_ms = 120000                      # 1..=600000
//! probe_failure_ttl_ms = 10000                     # 1..=600000
//! probe_cache_max_entries = 256                    # 1..=4096
//! probe_parallelism = 32                           # 1..=256
//!
//! [limits]
//! open_files = 8192                                # 256..=1048576
//! ```
//!
//! This crate validates allowlist patterns only syntactically; which names
//! the installer allowlists by default is decided by its caller.
//!
//! `[waits]` bounds `session.wait_turn`: `turn_default_ms` applies when a
//! caller omits its timeout, and `turn_max_ms` is the ceiling a requested
//! timeout may not exceed. See [`WaitTiming`] and [`DEFAULT_WAIT_TIMING`].
//! The three optional `keepalive_*` keys configure the TCP keepalive of every
//! overlay control connection; see [`KeepaliveTiming`] and
//! [`DEFAULT_KEEPALIVE_TIMING`].
//!
//! `[hooks]` bounds how much of a failing worktree-create hook's stderr the
//! daemon keeps for the `session.new` warning. See
//! [`DEFAULT_HOOK_STDERR_TAIL_BYTES`] and [`MAX_HOOK_STDERR_TAIL_BYTES`].
//!
//! `hooks.required` lists the hook events whose failure aborts the operation
//! instead of becoming a warning; see [`RequiredHook`]. The key is optional,
//! the default is an empty list, and the writer omits it while it is empty, so
//! a previous release still reads the file unless the operator set it. A
//! previous release that reads a file with the key reports it as an unknown key
//! and does not enforce it.
//!
//! `[inventory]` bounds the version-probe cache behind `host.inspect`; see
//! [`InventoryProbe`] and [`DEFAULT_INVENTORY_PROBE`]. The table and each of its
//! keys are optional, a file that lacks them means the defaults, and the writer
//! omits the table while every key equals its default, so a previous release
//! still reads the file unless the operator set a key.
//!
//! `[worker_journals]` bounds how long the daemon keeps the journal directory of
//! a session that no longer exists; see [`DEFAULT_WORKER_JOURNAL_MAX_AGE`]. The
//! table and its `max_age_days` key are optional, a file that lacks them means
//! the default, and the writer omits the table while the key equals the
//! default, so a previous release still reads the file unless the operator set
//! it. A previous release that reads a file with the table reports it as an
//! unknown table and keeps the journals.
//!
//! # Compatibility
//!
//! `schema_version` changes only for a breaking change: a removed or renamed
//! key, a changed meaning or unit, or validation that rejects a file that was
//! valid before. A normal load accepts only [`SCHEMA_VERSION`].
//!
//! Unknown keys and tables do not break a load:
//!
//! - They are ignored and reported by [`ServiceConfig::unknown_keys`] with the
//!   key path, the line, and a "did you mean" hint when a known sibling key is
//!   within two edits. The daemon logs one warning per key at
//!   start; `pohunek service status`, `check` and `upgrade` and `pohunek
//!   doctor` report them. Their values are never read into a message, a log
//!   line, or `Debug` output, because a hand-edited file may hold a secret
//!   there.
//! - [`ServiceConfig::to_toml`] and [`ServiceConfig::write`] keep them at the
//!   same paths, after the typed values, so an upgrade, a reinstall or a
//!   rollback does not drop a setting this version does not own. A key this
//!   version writes is always rendered from its typed value.
//! - `[namespace]` and `[environment]` stay strict: an unknown key there is a
//!   [`ConfigError::Parse`] naming the key, because an older binary must never
//!   skip a setting that moves a root or narrows the environment. A new
//!   setting in those tables or among the top-level identity keys (`prefix`,
//!   `active_version`) therefore needs a new schema version.
//! - A required key that is missing stays an error; when an unknown key of
//!   the same table resembles it the error names it. An invalid value of a
//!   known key fails as before, whatever unknown keys surround it.
//!
//! An additive setting elsewhere is an optional key with a documented
//! default and needs no schema bump. The writer omits it while it equals that
//! default, so a release that adds one still writes files the previous
//! release can read, unless the operator set the new key.
//!
//! `[events]` bounds the daemon's event journal: a file rotates before it would
//! exceed `max_file_bytes`, at most `max_files` files (the active one included)
//! are kept, and a rotated file whose last write is older than `max_age_days`
//! is pruned when the daemon starts and after each rotation; an active file whose
//! last write is older is removed when the daemon starts. See
//! [`EventRetention`] and [`DEFAULT_EVENT_RETENTION`]. The first start with
//! these bounds removes, without reading it, a journal file above
//! `max_file_bytes` left by an earlier release.
//!
//! # Upgrades
//!
//! A normal load accepts only [`SCHEMA_VERSION`]. The installer reads the
//! previous schemas through [`ServiceConfig::load_upgrade_source`], which
//! assigns the documented defaults to the keys those schemas lack: schema 2
//! has neither `[input]` nor `[waits]`, schema 3 has no `[waits]` and no
//! `input.write_stall_timeout_ms`, schema 4 has no
//! `input.write_stall_timeout_ms`, and none of them nor schema 5 has
//! `[hooks]`, and none of schemas 2 to 6 has `[events]`. Every value a previous file records is kept. The previous
//! schemas are read in their exact shapes: no release that wrote one could
//! write an unknown key, so an unknown key in such a file is refused.
//!
//! `environment.search_path` is the executable search path the installer
//! resolved (see `pohunek_platform::shell_env`) and hands to the daemon job as
//! its `PATH`. Each entry is an absolute, normalized directory without `:` or
//! control characters, no entry repeats, and the joined value stays within
//! [`MAX_SEARCH_PATH_BYTES`]. An empty list means the job keeps the service
//! manager's own `PATH`. Upgrades preserve the recorded list. The daemon does
//! not read it from the file: it reaches the job definition only when the
//! installer writes one (install, or an upgrade to a different version).
//!
//! # File trust
//!
//! The file must be a regular `0600` file owned by the effective user, with a
//! single link and no ACL granting access beyond its mode bits. Its directory
//! must be owned by the effective user and must not be group- or
//! world-writable, so both `0700` and the common `0755` config directory
//! pass. Integrity rests on that directory: nobody else can replace or rename
//! the file. Confidentiality rests on the `0600` file alone, which is why a
//! readable directory is acceptable. Symlinks and foreign owners are rejected
//! before any byte is parsed. Reads are bounded by [`MAX_CONFIG_BYTES`].
//! [`ServiceConfig::write`] replaces the file atomically through the same
//! trusted directory, creates a missing directory as `0700`, and never changes
//! the mode of an existing one.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//!
//! use pohunek_service_config::ServiceConfig;
//!
//! let config = ServiceConfig::load(Path::new("/home/u/.config/pohunek/service.toml"))?;
//! config.verify_installation(
//!     1000,
//!     Path::new("/home/u/.local/state/pohunek"),
//!     Path::new("/run/user/1000/pohunek"),
//! )?;
//! println!("daemon label: {}", config.namespace().daemon_label());
//! # Ok::<(), pohunek_service_config::ConfigError>(())
//! ```

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_paths::{
    is_normalized_absolute, valid_install_version, BasePaths, InstallLayout,
    MAX_INSTALL_VERSION_BYTES,
};
use pohunek_platform::filesystem::{FsError, TrustedDir};
use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::{
    Namespace, MAX_JOB_TIMEOUT, MAX_JOB_VALUE_BYTES, MAX_OPEN_FILES, MIN_OPEN_FILES,
};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, ImDocument, Item, Table, TableLike};

mod error;
pub mod preflight;
pub mod runtimes;

#[doc(inline)]
pub use error::ConfigError;

/// The only schema version this crate reads and writes.
///
/// A file with any other value is rejected before its keys are interpreted,
/// so an older binary never half-understands a newer installation and a newer
/// binary never reads a file that lacks keys it requires. Version 3 holds
/// `[input]`; version 4 adds `[waits]`; version 5 adds
/// `input.write_stall_timeout_ms`; version 6 adds `[hooks]`; version 7 adds
/// `[events]`. Schemas 2 to 6 are read only through
/// [`ServiceConfig::load_upgrade_source`].
pub const SCHEMA_VERSION: u32 = 7;

/// Input timings written on fresh installs and assigned explicitly when
/// upgrading a schema-2, schema-3 or schema-4 service file, which lack some
/// input keys.
///
/// The write stall bound is 3000 ms, below the 5 s default request timeout of
/// the client (`pohunek-client`). A fire-and-forget `session.input` caller then
/// receives the typed `session_input_stalled` error before its own timeout
/// fires. A reading agent drains its input within microseconds, so 3 s without
/// a single accepted byte means the reader is stuck, not slow.
pub const DEFAULT_INPUT_TIMING: InputTiming = InputTiming {
    initial_startup_grace: Duration::from_secs(5),
    submit_delay: Duration::from_millis(150),
    write_stall_timeout: Duration::from_millis(3000),
};

/// Turn-wait timings written on fresh installs, assigned when upgrading a
/// schema-2 or schema-3 file, and used by a daemon without `service.toml`.
///
/// The five-minute default lets an agent that omits a timeout follow a
/// typical agent turn in one call while still getting a progress report often
/// enough to notice a stuck session. The thirty-minute ceiling covers long
/// turns (large builds, test suites) in one wait, and bounds how long one
/// waiter slot stays taken: a client that disappears without closing its
/// connection, such as a vanished overlay host, holds its slot until the
/// timeout. Raising the ceiling lengthens that worst case; lowering the
/// default makes callers that omit a timeout poll more often.
pub const DEFAULT_WAIT_TIMING: WaitTiming = WaitTiming {
    turn_default: Duration::from_mins(5),
    turn_max: Duration::from_mins(30),
    keepalive: DEFAULT_KEEPALIVE_TIMING,
};

/// TCP keepalive of an overlay control connection when `[waits]` names none.
///
/// A client host that vanishes (network loss, suspend) sends neither data nor
/// a FIN, so a wait it started would hold its waiter slot until its own
/// deadline. Thirty seconds of silence, then three unanswered probes ten
/// seconds apart, end the connection about a minute after the peer went away
/// (`idle + interval * count`) while costing a healthy idle connection one
/// small probe per half minute. A shorter idle time would probe the
/// connections of agents that merely think for a while; a longer one lets a
/// dead peer keep a slot noticeably longer.
pub const DEFAULT_KEEPALIVE_TIMING: KeepaliveTiming = KeepaliveTiming {
    idle: Duration::from_secs(30),
    interval: Duration::from_secs(10),
    count: 3,
};

/// Stderr tail size written on fresh installs, assigned when upgrading a
/// schema-2 to schema-5 file, and used by a daemon without `service.toml`.
///
/// 4096 bytes hold the last few dozen lines of a typical tool error (a failed
/// `npm install`, `git` or compiler run), which is what tells the owner why
/// project setup failed, while the warning stays a small JSON value.
pub const DEFAULT_HOOK_STDERR_TAIL_BYTES: u32 = 4096;

/// Largest accepted `hooks.stderr_tail_bytes`.
///
/// One `session.new` result can carry four tails (`pre-create` and
/// `post-create`, each from the host-global and the in-repo layer). Lossy
/// UTF-8 decoding turns one invalid byte into a three-byte replacement
/// character, so four 32 KiB tails stay below 384 KiB of text, well within the
/// 1 MiB control line together with the rest of the session. 32 KiB is also
/// several hundred lines, more than anyone reads in a warning.
pub const MAX_HOOK_STDERR_TAIL_BYTES: u32 = 32 * 1024;

/// Event-journal retention written on fresh installs, assigned when upgrading
/// a schema-2 to schema-6 file, and used by a daemon without `service.toml`.
///
/// Eight 32 MiB files cap the journal at 256 MiB, the same family bound as
/// `pohunekd.jsonl`. A journal without heartbeat refreshes grows by a few tens
/// of megabytes per day, so 14 days of rotated files fit well inside it.
pub const DEFAULT_EVENT_RETENTION: EventRetention = EventRetention {
    max_file_bytes: 32 * 1024 * 1024,
    max_files: 8,
    max_age: Duration::from_secs(14 * SECONDS_PER_DAY),
};

/// How long the journal directory of a session that no longer exists is kept
/// before the daemon's startup collection may delete it, used when
/// `[worker_journals]` names none.
///
/// The journals of a removed session are evidence for cleaning up its worker
/// and for a replayed removal. Seven days is long enough for the owner to
/// notice and investigate a failed removal or a crashed worker, and short
/// enough that orphans of a busy host do not accumulate for weeks. Raising it
/// keeps the evidence longer; lowering it frees the disk sooner but narrows the
/// forensic window.
pub const DEFAULT_WORKER_JOURNAL_MAX_AGE: Duration = Duration::from_secs(7 * SECONDS_PER_DAY);

/// Smallest accepted `worker_journals.max_age_days`.
pub const MIN_WORKER_JOURNAL_AGE_DAYS: u64 = 1;

/// Largest accepted `worker_journals.max_age_days`.
pub const MAX_WORKER_JOURNAL_AGE_DAYS: u64 = 365;

/// Seconds in one day, the unit of `events.max_age_days`.
const SECONDS_PER_DAY: u64 = 86_400;

/// Smallest accepted `events.max_file_bytes`.
///
/// A control-plane event carries a full session record, up to a few hundred
/// kilobytes; a file below this could not hold one, and the journal would
/// replace such an event with a notice.
pub const MIN_EVENT_FILE_BYTES: u64 = 1024 * 1024;

/// Largest accepted `events.max_file_bytes`.
pub const MAX_EVENT_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Smallest accepted `events.max_files`, so a rotation keeps the previous file.
pub const MIN_EVENT_FILES: u32 = 2;

/// Largest accepted `events.max_files`.
pub const MAX_EVENT_FILES: u32 = 64;

/// Smallest accepted `events.max_age_days`.
pub const MIN_EVENT_AGE_DAYS: u64 = 1;

/// Largest accepted `events.max_age_days`: ten years.
pub const MAX_EVENT_AGE_DAYS: u64 = 3650;

/// Version-probe cache settings used when `[inventory]` names none.
///
/// A successful probe stays valid for two minutes: long enough that a client
/// reading `host.inspect` repeatedly (a launcher dialog, a snapshot reseed)
/// never pays for a probe, short enough that an interpreter or a dependency
/// upgraded under an unchanged script is noticed within minutes. A failed
/// probe is retried after ten seconds so a transient failure does not hide a
/// runtime for long. 256 entries cover several hundred runtimes and profiles
/// that share programs. 32 probes run at once, which keeps the cold inventory
/// of a host with dozens of distinct programs within one probe timeout.
pub const DEFAULT_INVENTORY_PROBE: InventoryProbe = InventoryProbe {
    cache_ttl: Duration::from_secs(120),
    failure_ttl: Duration::from_secs(10),
    cache_max_entries: 256,
    parallelism: 32,
};

/// Smallest accepted `inventory.probe_cache_max_entries`.
pub const MIN_PROBE_CACHE_ENTRIES: u32 = 1;

/// Largest accepted `inventory.probe_cache_max_entries`.
pub const MAX_PROBE_CACHE_ENTRIES: u32 = 4096;

/// Smallest accepted `inventory.probe_parallelism`.
pub const MIN_PROBE_PARALLELISM: u32 = 1;

/// Largest accepted `inventory.probe_parallelism`.
///
/// Each running probe holds one thread and a few descriptors; 256 stays far
/// below the open-file limit every supervised job gets.
pub const MAX_PROBE_PARALLELISM: u32 = 256;

/// Shortest accepted `[waits]` value.
///
/// Turn events are counted only after the attention debounce (5 s by
/// default), so a shorter wait would time out before a turn that ends during
/// it could count. A smaller value is most likely seconds written as
/// milliseconds.
pub const MIN_TURN_WAIT: Duration = Duration::from_secs(5);

/// Longest accepted `[waits]` value.
///
/// One hour is twice the default ceiling, room for an operator whose agents
/// run longer turns, while a waiter slot of a client that vanished without
/// closing its connection is still released within the hour. It is not
/// [`MAX_DEADLINE`]: turn waits are not native job timeouts.
pub const MAX_TURN_WAIT: Duration = Duration::from_hours(1);

/// Shortest accepted `waits.keepalive_idle_ms` and `waits.keepalive_interval_ms`.
///
/// The kernel options take whole seconds, so one second is both the unit and
/// the smallest value that is not a zero (which the kernel rejects).
pub const MIN_KEEPALIVE_TIME: Duration = Duration::from_secs(1);

/// Longest accepted `waits.keepalive_idle_ms` and `waits.keepalive_interval_ms`.
///
/// Ten minutes: a longer probe schedule no longer bounds a waiter slot in a
/// useful time next to the one-hour [`MAX_TURN_WAIT`], and it stays far below
/// the largest value the Linux kernel accepts (32767 seconds).
pub const MAX_KEEPALIVE_TIME: Duration = Duration::from_mins(10);

/// Smallest accepted `waits.keepalive_count`.
pub const MIN_KEEPALIVE_COUNT: u32 = 1;

/// Largest accepted `waits.keepalive_count`.
///
/// Ten unanswered probes already mean a dead peer; the kernel accepts up to
/// 127 on Linux.
pub const MAX_KEEPALIVE_COUNT: u32 = 10;

/// File name of the configuration inside the application config directory.
pub const FILE_NAME: &str = "service.toml";

/// Largest accepted configuration file in bytes.
///
/// A complete file with a 256-entry allowlist stays below 8 KiB; the bound
/// only keeps a corrupted or hostile file from being read into memory whole.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Longest accepted deadline or grace period.
///
/// Equal to the supervisor's job timeout ceiling because the exit timeouts
/// and restart throttle flow into native job definitions that reject longer
/// values; the other deadlines share it since anything longer would only hide
/// a wedged operation from the operator.
pub const MAX_DEADLINE: Duration = MAX_JOB_TIMEOUT;

/// Longest silent-reader wait for clients that omit the extended-wait opt-in.
///
/// A five-second client response deadline needs a short fallback so a silent
/// reader does not consume the entire budget before input is delivered.
pub const COMPAT_INITIAL_INPUT_STARTUP_GRACE: Duration = Duration::from_millis(500);

/// Default PTY stop grace of daemon and session workers.
///
/// The grace is how long a worker waits for the PTY to reach EOF after it
/// signals the session's process group with `SIGTERM`, and then once more after
/// `SIGKILL`, before it force-closes the PTY; only a root still alive then is
/// awaited longer, up to [`DEFAULT_KILL_REAP_DEADLINE`]. 500 ms is long enough for
/// a shell or an agent CLI on a loaded host to handle `SIGTERM` and flush its
/// final output, and short enough that an explicit stop returns in about a
/// second. Raising it delays every stop of a stuck process; lowering it risks a
/// final screen cut short by the force-close.
///
/// This is not a `service.toml` key. The daemon is the single authority: it
/// sends its grace to each worker in the `Initialize` stop policy (the wire
/// value is a non-zero number of milliseconds), and a worker only falls back to
/// this value before it has been initialized. Both sides take their default from
/// this constant, so they cannot drift apart, and tests that need another value
/// set the `stop_grace` field of the registry or worker configuration. The value
/// stays below [`MAX_DEADLINE`].
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_millis(500);

/// Default deadline of daemon and session workers for the death of a stopped
/// session's root process after `SIGKILL`.
///
/// `SIGKILL` cannot be caught or ignored, so a root that has not exited one
/// [`DEFAULT_STOP_GRACE`] after it is only held by the kernel: by uninterruptible
/// I/O, by tearing down a large address space, or by a host too loaded to
/// schedule the exit. A real agent CLI at a prompt can need more than one grace
/// for that on a busy CI runner. Three seconds covers such a host many times
/// over; a root still alive after it is stuck in the kernel, and the stop
/// reports it with the state it is in. Together with the grace before
/// `SIGKILL` that report arrives well inside the daemon's bound on a worker's
/// stop answer.
///
/// Like the grace, this is not a `service.toml` key: the daemon sends it to
/// each worker in the `Initialize` stop policy, and a worker falls back to this
/// value when the policy does not carry it. It is never shorter than
/// [`DEFAULT_STOP_GRACE`] and stays below [`MAX_DEADLINE`].
pub const DEFAULT_KILL_REAP_DEADLINE: Duration = Duration::from_secs(3);

/// Default wait of a session worker for the durable commit of a stopped
/// session's terminal outcome before it answers a stop with a retryable
/// refusal.
///
/// The daemon bounds its own wait for the worker's stop answer by this value
/// ([`DEFAULT_WORKER_STOP_HEADROOM`] on top of the worker's grace, kill reap
/// and sweep waits), so the two sides share one constant. Like the grace, it is
/// not a `service.toml` key.
pub const DEFAULT_TERMINAL_COMMIT_WAIT: Duration = Duration::from_secs(10);

/// Margin the daemon adds to the worker's own stop bounds before it gives up on
/// a worker that has not answered a stop.
///
/// It covers the control-connection round trip and scheduling on a loaded
/// host; a worker that is still silent after every bound of its own plus this
/// margin is frozen, not slow.
pub const DEFAULT_WORKER_STOP_HEADROOM: Duration = Duration::from_secs(2);

/// Longest a stop waits behind an input write that holds the worker's control
/// connection.
///
/// The worker delivers an input on its own stall policy, which restarts after
/// every byte the agent reads, so a large input read slowly legitimately holds
/// the connection for a long time and the daemon cannot see its progress. Eight
/// [`MAX_DEADLINE`] steps leave the SDK's stop budget (nine steps) room for the
/// stop itself; a worker frozen mid-input ends the stop with a typed error at
/// this bound instead of waiting for the caller's budget.
pub const DEFAULT_WORKER_INPUT_STOP_WAIT: Duration =
    MAX_DEADLINE.saturating_mul(WORKER_INPUT_STOP_WAIT_STEPS);

/// [`MAX_DEADLINE`] steps a stop may wait behind an input write: the SDK's stop
/// budget has nine steps and the stop itself needs one.
const WORKER_INPUT_STOP_WAIT_STEPS: u32 = 8;

/// How long the daemon waits for a session's worker to report its hook schema
/// when a hook reporter asked for the retryable answer. It stays below the
/// reporters' socket timeout so the typed refusal reaches a reporter that is
/// still listening; the reporter retries instead of the daemon waiting out
/// the worker connect deadline.
pub const DEFAULT_HOOK_ADMISSION_WAIT: Duration = Duration::from_millis(250);

/// First pause of a hook reporter between two delivery attempts while the
/// session's worker is unreachable.
pub const HOOK_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Longest pause between two delivery attempts of a hook reporter; the pause
/// doubles from [`HOOK_RETRY_BACKOFF`] up to this value.
pub const HOOK_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Time a hook reporter keeps for itself before the agent's hook timeout: one
/// socket timeout of its last attempt, process start and the final trace.
pub const HOOK_RETRY_MARGIN: Duration = Duration::from_secs(2);

/// Longest accepted path in bytes.
///
/// Equal to the supervisor's job value limit, because the prefix and roots
/// become executable paths, arguments, and bootstrap environment values.
pub const MAX_PATH_BYTES: usize = MAX_JOB_VALUE_BYTES;

/// Most entries the environment allowlist may hold.
///
/// Matches the worker protocol's bound on base-environment entries, so an
/// allowlist can never admit more variables than one `Initialize` carries.
pub const MAX_ALLOWLIST_ENTRIES: usize = 256;

/// Longest accepted allowlist pattern in bytes, including a trailing `*`.
///
/// Matches the worker protocol's bound on base-environment key length.
pub const MAX_PATTERN_BYTES: usize = 128;

/// Longest accepted joined `environment.search_path` in bytes.
///
/// Equal to the supervisor's job value limit, because the list becomes the
/// daemon job's `PATH` environment value.
pub const MAX_SEARCH_PATH_BYTES: usize = pohunek_platform::shell_env::MAX_SEARCH_PATH_BYTES;

/// Owner-only mode required of the configuration file.
const FILE_MODE: u32 = 0o600;

/// Mode of a configuration directory that [`ServiceConfig::write`] creates.
const DIRECTORY_MODE: u32 = 0o700;

/// Directory mode bits that would let another account replace the file.
///
/// Read and search bits for others are allowed: the `0600` file alone keeps
/// its contents private, while the absence of these bits keeps it intact.
const FORBIDDEN_DIRECTORY_BITS: u32 = 0o022;

/// Reserved `(uid_t)-1`, which `setuid`-family calls treat as "unchanged".
const INVALID_UID: u32 = u32::MAX;

/// Nanoseconds in one millisecond, the resolution of every recorded duration.
const NANOS_PER_MILLI: u32 = 1_000_000;

/// Header written above the generated TOML.
const FILE_HEADER: &str =
    "# Pohunek service configuration, written by `pohunek service install`.\n\
# Every key is required. Unknown keys are kept and reported as warnings,\n\
# except inside [namespace] and [environment], where they are rejected.\n\n";

/// The configuration text without the leading comment header and the blank
/// lines after it.
///
/// Rendered files start with a header whose wording differs between releases;
/// comparing two renderings by body ignores that wording.
#[must_use]
pub fn body_after_header(bytes: &[u8]) -> &[u8] {
    let mut rest = bytes;
    while let Some(line_end) = rest.iter().position(|b| *b == b'\n') {
        let line = &rest[..line_end];
        if line.first() != Some(&b'#') && !line.iter().all(u8::is_ascii_whitespace) {
            break;
        }
        rest = &rest[line_end + 1..];
    }
    rest
}

/// Largest edit distance at which an unknown key is offered a known key as a
/// "did you mean" hint.
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// Longest unknown key path, in bytes, that a warning repeats; longer paths
/// are cut so one hostile key cannot flood a log line or an error envelope.
const MAX_REPORTED_PATH_BYTES: usize = 128;

/// Per-process sequence making temporary file names unique between writes.
///
/// It only disambiguates names: the temporary file is created exclusively, so
/// a collision fails the write instead of corrupting another writer's file.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Returns `<config>/pohunek/service.toml` for resolved base paths.
#[must_use]
pub fn file_path(paths: &BasePaths) -> PathBuf {
    paths.config_dir.join(FILE_NAME)
}

/// Deadlines recorded in `[deadlines]`, each a whole number of milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Deadlines {
    /// How long the daemon waits for a new worker generation's socket.
    pub worker_connect: Duration,
    /// How long the daemon waits for a worker to answer `Initialize`.
    pub worker_initialize: Duration,
    /// How long one `launchctl` invocation may run.
    pub launchctl_command: Duration,
    /// How long a worker job may take to exit after a stop request.
    pub worker_exit_timeout: Duration,
    /// How long the daemon job may take to exit after a stop request.
    pub daemon_exit_timeout: Duration,
    /// Minimum interval between native restarts of a failed daemon.
    pub daemon_restart_throttle: Duration,
}

/// Host input timings recorded in `[input]`, in whole milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputTiming {
    /// Upper bound before initial input uses the startup fallback.
    pub initial_startup_grace: Duration,
    /// Delay between Claude's input body and separate submit write.
    pub submit_delay: Duration,
    /// Longest time one control input write may wait without the PTY accepting a byte.
    ///
    /// Restarted after every accepted byte, so it bounds a stalled reader, not
    /// the total duration of a long input. A reader that never drains its
    /// terminal would otherwise block the write, and every request queued
    /// behind it, indefinitely.
    pub write_stall_timeout: Duration,
}

/// Turn-wait timings recorded in `[waits]`, in whole milliseconds.
///
/// [`ServiceConfig::new`] keeps both within
/// [`MIN_TURN_WAIT`]`..=`[`MAX_TURN_WAIT`] and `turn_default` at most
/// `turn_max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaitTiming {
    /// Timeout of a `session.wait_turn` call that names none.
    pub turn_default: Duration,
    /// Longest timeout a `session.wait_turn` call may request.
    pub turn_max: Duration,
    /// TCP keepalive of every overlay control connection.
    pub keepalive: KeepaliveTiming,
}

/// TCP keepalive of an overlay control connection, recorded in `[waits]`.
///
/// A connection whose peer vanished without closing it is dropped, and the
/// waiter slot of its in-flight wait released, after at most
/// [`slot_lifetime_bound`](Self::slot_lifetime_bound) of silence.
/// [`ServiceConfig::new`] keeps `idle` and `interval` a whole number of
/// seconds within [`MIN_KEEPALIVE_TIME`]`..=`[`MAX_KEEPALIVE_TIME`] and
/// `count` within [`MIN_KEEPALIVE_COUNT`]`..=`[`MAX_KEEPALIVE_COUNT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeepaliveTiming {
    /// Silence on an established connection before the first probe.
    pub idle: Duration,
    /// Time between unanswered probes.
    pub interval: Duration,
    /// Unanswered probes after which the connection is dropped.
    pub count: u32,
}

impl KeepaliveTiming {
    /// How long a vanished peer can keep its connection: `idle + interval * count`.
    #[must_use]
    pub fn slot_lifetime_bound(&self) -> Duration {
        self.idle + self.interval * self.count
    }
}

/// Version-probe cache settings recorded in `[inventory]`.
///
/// [`ServiceConfig::new`] keeps both TTLs within the usual deadline bounds
/// (a whole number of milliseconds, `1..=600000`), `cache_max_entries` within
/// [`MIN_PROBE_CACHE_ENTRIES`]`..=`[`MAX_PROBE_CACHE_ENTRIES`] and
/// `parallelism` within [`MIN_PROBE_PARALLELISM`]`..=`[`MAX_PROBE_PARALLELISM`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InventoryProbe {
    /// How long a successful probe result is reused.
    pub cache_ttl: Duration,
    /// How long a failed or timed-out probe result is reused.
    pub failure_ttl: Duration,
    /// Most probe results the cache keeps; the oldest are dropped first.
    pub cache_max_entries: u32,
    /// Most probes a single inventory runs at once.
    pub parallelism: u32,
}

/// Event-journal retention recorded in `[events]`.
///
/// [`ServiceConfig::new`] keeps `max_file_bytes` within
/// [`MIN_EVENT_FILE_BYTES`]`..=`[`MAX_EVENT_FILE_BYTES`], `max_files` within
/// [`MIN_EVENT_FILES`]`..=`[`MAX_EVENT_FILES`], and `max_age` a whole number of
/// days within [`MIN_EVENT_AGE_DAYS`]`..=`[`MAX_EVENT_AGE_DAYS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventRetention {
    /// Largest size of one journal file; the journal rotates before a write
    /// would cross it.
    pub max_file_bytes: u64,
    /// Journal files kept, the active one included.
    pub max_files: u32,
    /// Age after which a rotated journal file is pruned, and after which the
    /// active file is removed at start.
    pub max_age: Duration,
}

/// A key or table of `service.toml` that this version does not know.
///
/// The value never enters this type, so no warning built from it can leak a
/// secret a hand-edited file holds. The path is escaped and bounded, so it is
/// safe to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    /// Dotted path of the key, such as `limits.open_file`.
    pub path: String,
    /// 1-based line of the key in the file it was read from.
    pub line: usize,
    /// Dotted path of the known key this one resembles, if any.
    pub suggestion: Option<String>,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown key `{}` (line {})", self.path, self.line)?;
        match &self.suggestion {
            Some(known) => write!(f, "; did you mean `{known}`?"),
            None => Ok(()),
        }
    }
}

/// The unknown keys of one loaded file and the items needed to write them back.
///
/// `kept` holds the outermost unknown items with the path of the known table
/// each sits in. Its `Debug` shows paths only, because the items hold the
/// values.
#[derive(Clone, Default, PartialEq, Eq)]
struct UnknownKeys {
    keys: Vec<UnknownKey>,
    kept: Vec<KeptItem>,
}

/// One unknown item and the known table it was found in.
#[derive(Clone)]
struct KeptItem {
    /// Names of the known tables from the document root down to the item.
    parents: Vec<String>,
    name: String,
    item: Item,
}

impl PartialEq for KeptItem {
    fn eq(&self, other: &Self) -> bool {
        self.parents == other.parents
            && self.name == other.name
            && self.item.to_string() == other.item.to_string()
    }
}

impl Eq for KeptItem {}

/// The kept unknown items could not be merged into a valid document.
///
/// Carries no text, because the parser's own error would quote the file.
#[derive(Debug)]
struct KeptMergeError;

impl std::fmt::Debug for UnknownKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.keys.iter().map(|key| &key.path))
            .finish()
    }
}

/// A validated service file read for an installer upgrade.
///
/// The source schema must be recorded with the upgrade transaction so a
/// rollback can restore the format understood by the previous daemon.
pub struct UpgradeSource {
    /// Current typed values, with the documented defaults assigned to the
    /// tables a previous schema lacks.
    pub config: ServiceConfig,
    /// Schema of the file before the upgrade.
    pub source_schema_version: u32,
    original_text: String,
}

impl std::fmt::Debug for UpgradeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeSource")
            .field("config", &self.config)
            .field("source_schema_version", &self.source_schema_version)
            .finish_non_exhaustive()
    }
}

impl UpgradeSource {
    /// Original validated bytes for binding a private backup to a transaction.
    ///
    /// The installer must not log these bytes and must verify their digest on
    /// resume before restoring a previous daemon's configuration.
    #[must_use]
    pub fn original_bytes(&self) -> &[u8] {
        self.original_text.as_bytes()
    }

    /// Atomically writes the original schema and values to a trusted path.
    ///
    /// Use this first to save the pre-upgrade file, then on rollback to
    /// restore it before starting the previous daemon.
    ///
    /// # Errors
    ///
    /// Returns the same path, trust, and write errors as [`ServiceConfig::write`].
    pub fn write_original(&self, path: &Path) -> Result<(), ConfigError> {
        write_config_text(path, &self.original_text)
    }
}

/// A lifecycle hook event whose failure aborts the operation it belongs to.
///
/// Only the events listed here can be named in `hooks.required`; every other
/// known event is refused at load as unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequiredHook {
    /// A failing `post-create` hook aborts `session.new` and rolls the create back.
    PostCreate,
}

impl RequiredHook {
    /// The event's name as `service.toml` and the hook file name spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostCreate => "post-create",
        }
    }
}

/// Every lifecycle hook event name, whether or not it can be required.
const HOOK_EVENT_NAMES: [&str; 7] = [
    "pre-create",
    "post-create",
    "pre-remove",
    "post-remove",
    "session-start",
    "session-stop",
    "agent-state",
];

/// Unvalidated values for [`ServiceConfig::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSpec {
    /// Absolute installation prefix.
    pub prefix: PathBuf,
    /// Version directory name the daemon and new workers run from.
    pub active_version: String,
    /// User owning the installation.
    pub uid: u32,
    /// Canonical application state directory (`<XDG_STATE_HOME>/pohunek`).
    pub state_root: PathBuf,
    /// Canonical application runtime directory (`<runtime>/pohunek`).
    pub runtime_root: PathBuf,
    /// Supervision deadlines.
    pub deadlines: Deadlines,
    /// Environment names or trailing-`*` prefixes a worker may forward.
    pub environment_allowlist: Vec<String>,
    /// Executable search path handed to the daemon job; empty keeps the
    /// service manager's own `PATH`.
    pub search_path: SearchPath,
    /// Delay between SIGTERM and SIGKILL when sweeping orphaned processes.
    pub sweep_grace: Duration,
    /// Initial input readiness and Claude submit timings.
    pub input_timing: InputTiming,
    /// Default and ceiling of a turn wait.
    pub wait_timing: WaitTiming,
    /// Bytes of a failing worktree-create hook's stderr kept for its warning;
    /// 0 disables the capture.
    pub hook_stderr_tail_bytes: u32,
    /// Hook events whose failure aborts the operation; each event at most once.
    pub hook_required: Vec<RequiredHook>,
    /// Size, count and age bounds of the event journal.
    pub event_retention: EventRetention,
    /// Version-probe cache of `host.inspect`.
    pub inventory_probe: InventoryProbe,
    /// Age after which the journal directory of a removed session is collected
    /// at daemon start; a whole number of days within
    /// [`MIN_WORKER_JOURNAL_AGE_DAYS`]`..=`[`MAX_WORKER_JOURNAL_AGE_DAYS`].
    pub worker_journal_max_age: Duration,
    /// Open-file limit applied to supervised jobs.
    pub open_files: u64,
}

/// A validated service configuration.
///
/// Every value satisfies the bounds documented on the crate constants, and
/// [`namespace`](Self::namespace) is derived from the recorded namespace inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    layout: InstallLayout,
    active_version: String,
    uid: u32,
    state_root: PathBuf,
    runtime_root: PathBuf,
    namespace: Namespace,
    deadlines: Deadlines,
    environment_allowlist: Vec<String>,
    search_path: SearchPath,
    sweep_grace: Duration,
    input_timing: InputTiming,
    wait_timing: WaitTiming,
    hook_stderr_tail_bytes: u32,
    hook_required: Vec<RequiredHook>,
    event_retention: EventRetention,
    inventory_probe: InventoryProbe,
    worker_journal_max_age: Duration,
    open_files: u64,
    unknown: UnknownKeys,
}

impl ServiceConfig {
    /// Validates `spec` and derives its namespace.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidPath`] for a relative, unnormalized,
    /// non-UTF-8, or oversized path; [`ConfigError::InvalidVersion`],
    /// [`ConfigError::InvalidUid`], [`ConfigError::OutOfRange`],
    /// [`ConfigError::Precision`], [`ConfigError::AllowlistSize`],
    /// [`ConfigError::InvalidPattern`], [`ConfigError::DuplicatePattern`], or
    /// [`ConfigError::WaitDefaultAboveMax`] for the respective invalid value.
    pub fn new(spec: ConfigSpec) -> Result<Self, ConfigError> {
        let layout = validate_prefix(&spec.prefix)?;
        if valid_install_version(&spec.active_version).is_none() {
            return Err(ConfigError::InvalidVersion {
                length: spec.active_version.len(),
                max_bytes: MAX_INSTALL_VERSION_BYTES,
            });
        }
        if spec.uid == INVALID_UID {
            return Err(ConfigError::InvalidUid { uid: spec.uid });
        }
        validate_path("namespace.state_root", &spec.state_root)?;
        validate_path("namespace.runtime_root", &spec.runtime_root)?;
        let deadlines = spec.deadlines;
        validate_duration("deadlines.worker_connect_ms", deadlines.worker_connect)?;
        validate_duration(
            "deadlines.worker_initialize_ms",
            deadlines.worker_initialize,
        )?;
        validate_duration(
            "deadlines.launchctl_command_ms",
            deadlines.launchctl_command,
        )?;
        validate_duration(
            "deadlines.worker_exit_timeout_ms",
            deadlines.worker_exit_timeout,
        )?;
        validate_duration(
            "deadlines.daemon_exit_timeout_ms",
            deadlines.daemon_exit_timeout,
        )?;
        validate_duration(
            "deadlines.daemon_restart_throttle_ms",
            deadlines.daemon_restart_throttle,
        )?;
        validate_allowlist(&spec.environment_allowlist)?;
        validate_duration("sweep.grace_ms", spec.sweep_grace)?;
        validate_duration(
            "input.initial_startup_grace_ms",
            spec.input_timing.initial_startup_grace,
        )?;
        validate_duration("input.submit_delay_ms", spec.input_timing.submit_delay)?;
        validate_duration(
            "input.write_stall_timeout_ms",
            spec.input_timing.write_stall_timeout,
        )?;
        validate_wait_timing(spec.wait_timing)?;
        if spec.hook_stderr_tail_bytes > MAX_HOOK_STDERR_TAIL_BYTES {
            return Err(ConfigError::OutOfRange {
                key: "hooks.stderr_tail_bytes",
                value: u64::from(spec.hook_stderr_tail_bytes),
                min: 0,
                max: u64::from(MAX_HOOK_STDERR_TAIL_BYTES),
            });
        }
        validate_required_hooks(&spec.hook_required)?;
        validate_event_retention(spec.event_retention)?;
        validate_inventory_probe(spec.inventory_probe)?;
        validate_worker_journal_max_age(spec.worker_journal_max_age)?;
        if !(MIN_OPEN_FILES..=MAX_OPEN_FILES).contains(&spec.open_files) {
            return Err(ConfigError::OutOfRange {
                key: "limits.open_files",
                value: spec.open_files,
                min: MIN_OPEN_FILES,
                max: MAX_OPEN_FILES,
            });
        }
        let namespace = Namespace::derive(spec.uid, &spec.state_root, &spec.runtime_root);
        Ok(Self {
            layout,
            active_version: spec.active_version,
            uid: spec.uid,
            state_root: spec.state_root,
            runtime_root: spec.runtime_root,
            namespace,
            deadlines,
            environment_allowlist: spec.environment_allowlist,
            search_path: spec.search_path,
            sweep_grace: spec.sweep_grace,
            input_timing: spec.input_timing,
            wait_timing: spec.wait_timing,
            hook_stderr_tail_bytes: spec.hook_stderr_tail_bytes,
            hook_required: spec.hook_required,
            event_retention: spec.event_retention,
            inventory_probe: spec.inventory_probe,
            worker_journal_max_age: spec.worker_journal_max_age,
            open_files: spec.open_files,
            unknown: UnknownKeys::default(),
        })
    }

    /// Loads and validates the configuration at an absolute `path`.
    ///
    /// The file is read through a trusted directory descriptor without
    /// following symlinks and parsed only after its type, owner, and mode are
    /// verified.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ConfigPath`] for a relative or unnormalized path,
    /// [`ConfigError::Untrusted`] for a symlink, special file, foreign owner,
    /// any file mode other than `0600`, or a group- or world-writable directory,
    /// [`ConfigError::TooLarge`] above [`MAX_CONFIG_BYTES`],
    /// [`ConfigError::Io`] when the file is missing or unreadable,
    /// [`ConfigError::Parse`] for invalid TOML, a missing, unknown, or mistyped
    /// key, [`ConfigError::UnsupportedSchema`] for another schema version, and
    /// every validation error of [`ServiceConfig::new`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        parse(path, &read_config_text(path)?)
    }

    /// Reads a current or a schema-6, -5, -4, -3, or -2 service file for an installer upgrade.
    ///
    /// Schema 2 has no `[input]` or `[waits]` table, schema 3 no `[waits]`
    /// and no `input.write_stall_timeout_ms`, schema 4 no
    /// `input.write_stall_timeout_ms`, schemas 2 to 5 no `[hooks]` table, and
    /// schemas 2 to 6 no `[events]` table. This method supplies the documented
    /// [`DEFAULT_INPUT_TIMING`], [`DEFAULT_WAIT_TIMING`],
    /// [`DEFAULT_HOOK_STDERR_TAIL_BYTES`] and [`DEFAULT_EVENT_RETENTION`]
    /// explicitly for the missing keys and tables and keeps every recorded
    /// value; normal daemon and worker loads still require every key of the
    /// current schema. The caller must preserve the source
    /// schema across the transaction and restore it if the old daemon is
    /// restarted on rollback.
    ///
    /// # Errors
    ///
    /// Returns the same trust and validation errors as [`Self::load`], and
    /// [`ConfigError::UnsupportedSchema`] for schemas other than 2, 3, 4, 5, 6, or
    /// [`SCHEMA_VERSION`].
    pub fn load_upgrade_source(path: &Path) -> Result<UpgradeSource, ConfigError> {
        Self::load_upgrade_source_bytes(path, read_config_text(path)?.as_bytes())
    }

    /// Parses already-read bytes as a service file for an installer upgrade.
    ///
    /// The bytes carry no trust on their own: the caller binds them first, for
    /// example to the digest a transaction record journals for the exact
    /// pre-upgrade file, and refuses a mismatch before using the result.
    /// `path` names the bytes in every error. `load_upgrade_source` is this
    /// function over a trusted read.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] for bytes that are not UTF-8, invalid
    /// TOML, or a missing, unknown, or mistyped key, and
    /// [`ConfigError::UnsupportedSchema`] for schemas other than 2, 3, 4, 5, 6, or
    /// [`SCHEMA_VERSION`].
    pub fn load_upgrade_source_bytes(
        path: &Path,
        bytes: &[u8],
    ) -> Result<UpgradeSource, ConfigError> {
        let original_text =
            String::from_utf8(bytes.to_vec()).map_err(|error| ConfigError::Parse {
                path: path.to_path_buf(),
                message: format!("file is not UTF-8: {error}"),
            })?;
        let (config, source_schema_version) = parse_upgrade_source(path, &original_text)?;
        Ok(UpgradeSource {
            config,
            source_schema_version,
            original_text,
        })
    }

    /// Atomically writes this configuration to an absolute `path` with mode `0600`.
    ///
    /// A missing parent directory is created with mode `0700`. An existing one
    /// must be owned by the effective user and not group- or world-writable;
    /// its mode is never changed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ConfigPath`] for a relative or unnormalized path,
    /// [`ConfigError::Untrusted`] when the directory is foreign-owned or
    /// group- or world-writable,
    /// [`ConfigError::Io`] when it cannot be opened or created,
    /// [`ConfigError::TooLarge`] when the rendering, with the unknown keys it
    /// keeps, exceeds [`MAX_CONFIG_BYTES`] (the file is left untouched), and
    /// [`ConfigError::Write`] when the replacement fails.
    pub fn write(&self, path: &Path) -> Result<(), ConfigError> {
        write_config_text(path, &self.to_toml())
    }

    /// Returns this configuration with only its active version changed.
    ///
    /// Everything else is kept, including the unknown keys of the file this
    /// value was loaded from, so a version change never drops a setting this
    /// version does not own.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidVersion`] for an invalid version.
    pub fn with_active_version(&self, version: &str) -> Result<Self, ConfigError> {
        let mut spec = self.to_spec();
        version.clone_into(&mut spec.active_version);
        let mut config = Self::new(spec)?;
        config.unknown = self.unknown.clone();
        Ok(config)
    }

    /// Returns the keys and tables of the loaded file that this version does
    /// not know, in file order.
    ///
    /// They were ignored when the file was loaded and are written back by
    /// [`to_toml`](Self::to_toml). A value built by [`ServiceConfig::new`] has none.
    #[must_use]
    pub fn unknown_keys(&self) -> &[UnknownKey] {
        &self.unknown.keys
    }

    /// Renders the configuration as the TOML document [`load`](Self::load) accepts.
    ///
    /// The unknown keys of the loaded file are merged back at their original
    /// paths after the typed values; a key this version writes is always
    /// rendered from its typed value.
    #[must_use]
    pub fn to_toml(&self) -> String {
        self.render()
            .expect("load checked that the kept unknown keys merge into the rendering")
    }

    fn render(&self) -> Result<String, KeptMergeError> {
        let raw = RawConfig {
            schema_version: SCHEMA_VERSION,
            prefix: utf8(self.layout.prefix()).to_owned(),
            active_version: self.active_version.clone(),
            namespace: RawNamespace {
                uid: self.uid,
                state_root: utf8(&self.state_root).to_owned(),
                runtime_root: utf8(&self.runtime_root).to_owned(),
            },
            deadlines: RawDeadlines {
                worker_connect_ms: millis(self.deadlines.worker_connect),
                worker_initialize_ms: millis(self.deadlines.worker_initialize),
                launchctl_command_ms: millis(self.deadlines.launchctl_command),
                worker_exit_timeout_ms: millis(self.deadlines.worker_exit_timeout),
                daemon_exit_timeout_ms: millis(self.deadlines.daemon_exit_timeout),
                daemon_restart_throttle_ms: millis(self.deadlines.daemon_restart_throttle),
            },
            environment: RawEnvironment {
                search_path: self
                    .search_path
                    .entries()
                    .iter()
                    .map(|entry| utf8(entry).to_owned())
                    .collect(),
                allowlist: self.environment_allowlist.clone(),
            },
            sweep: RawSweep {
                grace_ms: millis(self.sweep_grace),
            },
            input: RawInput {
                initial_startup_grace_ms: millis(self.input_timing.initial_startup_grace),
                submit_delay_ms: millis(self.input_timing.submit_delay),
                write_stall_timeout_ms: millis(self.input_timing.write_stall_timeout),
            },
            waits: RawWaits::from(self.wait_timing),
            hooks: RawHooks {
                stderr_tail_bytes: self.hook_stderr_tail_bytes,
                required: self
                    .hook_required
                    .iter()
                    .map(|hook| hook.as_str().to_owned())
                    .collect(),
            },
            events: RawEvents::from(self.event_retention),
            inventory: RawInventory::from(self.inventory_probe),
            worker_journals: RawWorkerJournals::from(self.worker_journal_max_age),
            limits: RawLimits {
                open_files: self.open_files,
            },
        };
        let body = toml::to_string(&raw)
            .expect("validated strings, bounded integers, and tables always serialize as TOML");
        let typed = format!("{FILE_HEADER}{body}");
        if self.unknown.kept.is_empty() {
            return Ok(typed);
        }
        merge_kept_keys(&typed, &self.unknown.kept)
    }

    /// Returns the values this configuration was built from.
    ///
    /// Useful to change one value, such as the active version on upgrade, and
    /// revalidate through [`ServiceConfig::new`].
    #[must_use]
    pub fn to_spec(&self) -> ConfigSpec {
        ConfigSpec {
            prefix: self.layout.prefix().to_path_buf(),
            active_version: self.active_version.clone(),
            uid: self.uid,
            state_root: self.state_root.clone(),
            runtime_root: self.runtime_root.clone(),
            deadlines: self.deadlines,
            environment_allowlist: self.environment_allowlist.clone(),
            search_path: self.search_path.clone(),
            sweep_grace: self.sweep_grace,
            input_timing: self.input_timing,
            wait_timing: self.wait_timing,
            hook_stderr_tail_bytes: self.hook_stderr_tail_bytes,
            hook_required: self.hook_required.clone(),
            event_retention: self.event_retention,
            inventory_probe: self.inventory_probe,
            worker_journal_max_age: self.worker_journal_max_age,
            open_files: self.open_files,
        }
    }

    /// Fails unless the running installation matches the recorded namespace inputs.
    ///
    /// `state_root` and `runtime_root` are the application state and runtime
    /// directories of the running process; both are canonicalized before the
    /// comparison, so they must exist. The daemon calls this at startup so a
    /// configuration written for another user or installation is never used to
    /// name, adopt, or retire jobs.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Canonicalize`] when a root cannot be resolved and
    /// [`ConfigError::NamespaceMismatch`] naming the first differing key.
    pub fn verify_installation(
        &self,
        uid: u32,
        state_root: &Path,
        runtime_root: &Path,
    ) -> Result<(), ConfigError> {
        if uid != self.uid {
            return Err(ConfigError::NamespaceMismatch {
                key: "namespace.uid",
                recorded: self.uid.to_string(),
                actual: uid.to_string(),
            });
        }
        verify_root("namespace.state_root", &self.state_root, state_root)?;
        verify_root("namespace.runtime_root", &self.runtime_root, runtime_root)
    }

    /// Returns the installation prefix.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        self.layout.prefix()
    }

    /// Returns the versioned executable layout below the prefix.
    #[must_use]
    pub fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    /// Returns the active version directory name.
    #[must_use]
    pub fn active_version(&self) -> &str {
        &self.active_version
    }

    /// Returns `<prefix>/libexec/pohunek/<active_version>`.
    #[must_use]
    pub fn version_dir(&self) -> PathBuf {
        self.layout
            .version_dir(&self.active_version)
            .expect("active_version was validated by ServiceConfig::new")
    }

    /// Returns the daemon executable of the active version.
    #[must_use]
    pub fn daemon_executable(&self) -> PathBuf {
        self.layout
            .daemon_executable(&self.active_version)
            .expect("active_version was validated by ServiceConfig::new")
    }

    /// Returns the session-worker executable of the active version.
    #[must_use]
    pub fn worker_executable(&self) -> PathBuf {
        self.layout
            .worker_executable(&self.active_version)
            .expect("active_version was validated by ServiceConfig::new")
    }

    /// Returns the recorded user id.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the recorded canonical application state directory.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Returns the recorded canonical application runtime directory.
    #[must_use]
    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    /// Returns the job namespace derived from the recorded namespace inputs.
    #[must_use]
    pub fn namespace(&self) -> Namespace {
        self.namespace.clone()
    }

    /// Returns the supervision deadlines.
    #[must_use]
    pub fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    /// Returns the environment allowlist patterns in file order.
    #[must_use]
    pub fn environment_allowlist(&self) -> &[String] {
        &self.environment_allowlist
    }

    /// Returns the executable search path recorded for the daemon job.
    ///
    /// Empty when the job keeps the service manager's own `PATH`.
    #[must_use]
    pub fn search_path(&self) -> &SearchPath {
        &self.search_path
    }

    /// Returns the orphan-sweep grace period between SIGTERM and SIGKILL.
    #[must_use]
    pub fn sweep_grace(&self) -> Duration {
        self.sweep_grace
    }

    /// Returns the host input timings.
    #[must_use]
    pub fn input_timing(&self) -> InputTiming {
        self.input_timing
    }

    /// Returns the default and ceiling of a turn wait.
    #[must_use]
    pub fn wait_timing(&self) -> WaitTiming {
        self.wait_timing
    }

    /// Returns how many bytes of a failing worktree-create hook's stderr the
    /// daemon keeps for its warning; 0 disables the capture.
    #[must_use]
    pub fn hook_stderr_tail_bytes(&self) -> u32 {
        self.hook_stderr_tail_bytes
    }

    /// Returns the hook events whose failure aborts the operation; empty when
    /// `hooks.required` is absent.
    #[must_use]
    pub fn hook_required(&self) -> &[RequiredHook] {
        &self.hook_required
    }

    /// Returns the size, count and age bounds of the event journal.
    #[must_use]
    pub fn event_retention(&self) -> EventRetention {
        self.event_retention
    }

    /// Version-probe cache settings of `host.inspect`.
    #[must_use]
    pub fn inventory_probe(&self) -> InventoryProbe {
        self.inventory_probe
    }

    /// Age after which the journal directory of a removed session is collected
    /// at daemon start.
    #[must_use]
    pub fn worker_journal_max_age(&self) -> Duration {
        self.worker_journal_max_age
    }

    /// Returns the open-file limit for supervised jobs.
    #[must_use]
    pub fn open_files(&self) -> u64 {
        self.open_files
    }
}

/// Reads a trusted service file while keeping parsing policy at the caller.
fn read_config_text(path: &Path) -> Result<String, ConfigError> {
    let (parent, name) = split_config_path(path)?;
    let directory = TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS)
        .map_err(|source| classify_read_error(path, source))?;
    let bytes = directory
        .read_file(name, FILE_MODE, MAX_CONFIG_BYTES)
        .map_err(|source| classify_read_error(path, source))?;
    String::from_utf8(bytes).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: format!("file is not UTF-8: {error}"),
    })
}

/// Replaces a trusted service configuration or transaction backup atomically.
fn write_config_text(path: &Path, text: &str) -> Result<(), ConfigError> {
    // Kept unknown keys can make a rewrite larger than the file it came from;
    // a file above the load bound would replace a working one with one no
    // daemon can read.
    if text.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
            max_bytes: MAX_CONFIG_BYTES,
        });
    }
    let (parent, name) = split_config_path(path)?;
    let directory =
        open_or_create_directory(parent).map_err(|source| classify_read_error(path, source))?;
    let temporary = format!(
        ".{FILE_NAME}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    directory
        .replace_file(name, temporary, text.as_bytes(), FILE_MODE)
        .map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// On-disk form of the whole file.
///
/// Unknown keys are ignored by serde here and reported by [`scan_unknown`];
/// `[namespace]` and `[environment]` keep `deny_unknown_fields` because an
/// older binary must never skip a setting that moves a root or narrows the
/// environment.
#[derive(Debug, Serialize, Deserialize)]
struct RawConfig {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInput,
    waits: RawWaits,
    hooks: RawHooks,
    events: RawEvents,
    #[serde(default, skip_serializing_if = "RawInventory::is_default")]
    inventory: RawInventory,
    #[serde(default, skip_serializing_if = "RawWorkerJournals::is_default")]
    worker_journals: RawWorkerJournals,
    limits: RawLimits,
}

/// Exact on-disk schema-6 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV6 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInput,
    waits: RawWaits,
    hooks: RawHooks,
    limits: RawLimits,
}

/// Exact on-disk schema-5 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV5 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInput,
    waits: RawWaits,
    limits: RawLimits,
}

/// Exact on-disk schema-3 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV3 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInputWithoutStall,
    limits: RawLimits,
}

/// Exact on-disk schema-4 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV4 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInputWithoutStall,
    waits: RawWaits,
    limits: RawLimits,
}

/// Exact on-disk schema-2 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV2 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    limits: RawLimits,
}

/// On-disk `[namespace]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNamespace {
    uid: u32,
    state_root: String,
    runtime_root: String,
}

/// On-disk `[deadlines]` table in milliseconds.
#[derive(Debug, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the `_ms` suffix is the documented on-disk unit of every key"
)]
struct RawDeadlines {
    worker_connect_ms: u64,
    worker_initialize_ms: u64,
    launchctl_command_ms: u64,
    worker_exit_timeout_ms: u64,
    daemon_exit_timeout_ms: u64,
    daemon_restart_throttle_ms: u64,
}

/// On-disk `[environment]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvironment {
    search_path: Vec<String>,
    allowlist: Vec<String>,
}

/// On-disk `[sweep]` table.
#[derive(Debug, Serialize, Deserialize)]
struct RawSweep {
    grace_ms: u64,
}

/// On-disk `[input]` table in milliseconds.
#[derive(Debug, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the `_ms` suffix is the documented on-disk unit of every key"
)]
struct RawInput {
    initial_startup_grace_ms: u64,
    submit_delay_ms: u64,
    write_stall_timeout_ms: u64,
}

/// On-disk `[input]` table of schemas 3 and 4, which has no write stall bound.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputWithoutStall {
    initial_startup_grace_ms: u64,
    submit_delay_ms: u64,
}

/// On-disk `[waits]` table in milliseconds.
///
/// The keepalive keys are optional: a file that lacks one means its default,
/// and the writer omits one that equals its default, so a file written
/// without a deliberate keepalive setting stays readable by a release that
/// predates the keys.
#[derive(Debug, Serialize, Deserialize)]
struct RawWaits {
    turn_default_ms: u64,
    turn_max_ms: u64,
    #[serde(
        default = "default_keepalive_idle_ms",
        skip_serializing_if = "is_default_keepalive_idle_ms"
    )]
    keepalive_idle_ms: u64,
    #[serde(
        default = "default_keepalive_interval_ms",
        skip_serializing_if = "is_default_keepalive_interval_ms"
    )]
    keepalive_interval_ms: u64,
    #[serde(
        default = "default_keepalive_count",
        skip_serializing_if = "is_default_keepalive_count"
    )]
    keepalive_count: u32,
}

impl From<WaitTiming> for RawWaits {
    fn from(timing: WaitTiming) -> Self {
        Self {
            turn_default_ms: millis(timing.turn_default),
            turn_max_ms: millis(timing.turn_max),
            keepalive_idle_ms: millis(timing.keepalive.idle),
            keepalive_interval_ms: millis(timing.keepalive.interval),
            keepalive_count: timing.keepalive.count,
        }
    }
}

fn default_keepalive_idle_ms() -> u64 {
    millis(DEFAULT_KEEPALIVE_TIMING.idle)
}

fn default_keepalive_interval_ms() -> u64 {
    millis(DEFAULT_KEEPALIVE_TIMING.interval)
}

fn default_keepalive_count() -> u32 {
    DEFAULT_KEEPALIVE_TIMING.count
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_keepalive_idle_ms(value: &u64) -> bool {
    *value == default_keepalive_idle_ms()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_keepalive_interval_ms(value: &u64) -> bool {
    *value == default_keepalive_interval_ms()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_keepalive_count(value: &u32) -> bool {
    *value == default_keepalive_count()
}

/// On-disk `[inventory]` table in milliseconds.
///
/// Every key is optional: a file that lacks one means its default, and the
/// writer omits a key that equals its default, so a file written without a
/// deliberate setting stays readable by a release that predates the table.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the keys are the four probe settings"
)]
struct RawInventory {
    #[serde(
        default = "default_probe_cache_ttl_ms",
        skip_serializing_if = "is_default_probe_cache_ttl_ms"
    )]
    probe_cache_ttl_ms: u64,
    #[serde(
        default = "default_probe_failure_ttl_ms",
        skip_serializing_if = "is_default_probe_failure_ttl_ms"
    )]
    probe_failure_ttl_ms: u64,
    #[serde(
        default = "default_probe_cache_max_entries",
        skip_serializing_if = "is_default_probe_cache_max_entries"
    )]
    probe_cache_max_entries: u32,
    #[serde(
        default = "default_probe_parallelism",
        skip_serializing_if = "is_default_probe_parallelism"
    )]
    probe_parallelism: u32,
}

/// The optional `[worker_journals]` table: its key is optional and omitted
/// while it equals its default.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RawWorkerJournals {
    #[serde(
        default = "default_worker_journal_max_age_days",
        skip_serializing_if = "is_default_worker_journal_max_age_days"
    )]
    max_age_days: u64,
}

impl RawWorkerJournals {
    /// Whether the key equals its default, so the whole table is omitted.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A value of the key that is not its default, used only to derive the set
    /// of known keys.
    fn known_keys_sample() -> Self {
        Self {
            max_age_days: default_worker_journal_max_age_days() + 1,
        }
    }
}

impl Default for RawWorkerJournals {
    fn default() -> Self {
        Self::from(DEFAULT_WORKER_JOURNAL_MAX_AGE)
    }
}

impl From<Duration> for RawWorkerJournals {
    fn from(max_age: Duration) -> Self {
        Self {
            max_age_days: max_age.as_secs() / SECONDS_PER_DAY,
        }
    }
}

fn default_worker_journal_max_age_days() -> u64 {
    DEFAULT_WORKER_JOURNAL_MAX_AGE.as_secs() / SECONDS_PER_DAY
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_worker_journal_max_age_days(value: &u64) -> bool {
    *value == default_worker_journal_max_age_days()
}

impl RawInventory {
    /// Whether every key equals its default, so the whole table is omitted.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A value of every key, none at its default, used only to derive the set
    /// of known keys.
    fn known_keys_sample() -> Self {
        Self {
            probe_cache_ttl_ms: default_probe_cache_ttl_ms() + 1,
            probe_failure_ttl_ms: default_probe_failure_ttl_ms() + 1,
            probe_cache_max_entries: default_probe_cache_max_entries() + 1,
            probe_parallelism: default_probe_parallelism() + 1,
        }
    }
}

impl Default for RawInventory {
    fn default() -> Self {
        Self::from(DEFAULT_INVENTORY_PROBE)
    }
}

impl From<InventoryProbe> for RawInventory {
    fn from(probe: InventoryProbe) -> Self {
        Self {
            probe_cache_ttl_ms: millis(probe.cache_ttl),
            probe_failure_ttl_ms: millis(probe.failure_ttl),
            probe_cache_max_entries: probe.cache_max_entries,
            probe_parallelism: probe.parallelism,
        }
    }
}

fn default_probe_cache_ttl_ms() -> u64 {
    millis(DEFAULT_INVENTORY_PROBE.cache_ttl)
}

fn default_probe_failure_ttl_ms() -> u64 {
    millis(DEFAULT_INVENTORY_PROBE.failure_ttl)
}

fn default_probe_cache_max_entries() -> u32 {
    DEFAULT_INVENTORY_PROBE.cache_max_entries
}

fn default_probe_parallelism() -> u32 {
    DEFAULT_INVENTORY_PROBE.parallelism
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_probe_cache_ttl_ms(value: &u64) -> bool {
    *value == default_probe_cache_ttl_ms()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_probe_failure_ttl_ms(value: &u64) -> bool {
    *value == default_probe_failure_ttl_ms()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_probe_cache_max_entries(value: &u32) -> bool {
    *value == default_probe_cache_max_entries()
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_default_probe_parallelism(value: &u32) -> bool {
    *value == default_probe_parallelism()
}

/// On-disk `[hooks]` table.
#[derive(Debug, Serialize, Deserialize)]
struct RawHooks {
    stderr_tail_bytes: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required: Vec<String>,
}

impl RawHooks {
    /// The documented value assigned to a previous schema, which has no `[hooks]`.
    const DEFAULT: Self = Self {
        stderr_tail_bytes: DEFAULT_HOOK_STDERR_TAIL_BYTES,
        required: Vec::new(),
    };
}

/// On-disk `[events]` table.
#[derive(Debug, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "the keys are the three retention limits"
)]
struct RawEvents {
    max_file_bytes: u64,
    max_files: u32,
    max_age_days: u64,
}

impl RawEvents {
    /// The documented value assigned to a previous schema, which has no `[events]`.
    const DEFAULT: Self = Self {
        max_file_bytes: DEFAULT_EVENT_RETENTION.max_file_bytes,
        max_files: DEFAULT_EVENT_RETENTION.max_files,
        max_age_days: DEFAULT_EVENT_RETENTION.max_age.as_secs() / SECONDS_PER_DAY,
    };
}

impl From<EventRetention> for RawEvents {
    fn from(retention: EventRetention) -> Self {
        Self {
            max_file_bytes: retention.max_file_bytes,
            max_files: retention.max_files,
            max_age_days: retention.max_age.as_secs() / SECONDS_PER_DAY,
        }
    }
}

/// On-disk `[limits]` table.
#[derive(Debug, Serialize, Deserialize)]
struct RawLimits {
    open_files: u64,
}

/// Parses and validates the text of a configuration read from `path`.
fn parse(path: &Path, text: &str) -> Result<ServiceConfig, ConfigError> {
    let parse_error = |error: toml::de::Error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: diagnostic_with_hint(path, text, &error),
    };
    // The schema version is checked before the typed parse, so a newer file
    // reports its version instead of its first key this schema does not know.
    let table: toml::Table = toml::from_str(text).map_err(parse_error)?;
    if let Some(toml::Value::Integer(found)) = table.get("schema_version") {
        if *found != i64::from(SCHEMA_VERSION) {
            return Err(ConfigError::UnsupportedSchema { found: *found });
        }
    }
    let raw: RawConfig = toml::from_str(text).map_err(parse_error)?;
    if raw.schema_version != SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchema {
            found: i64::from(raw.schema_version),
        });
    }
    let mut config = config_from_raw(raw)?;
    config.unknown = scan_unknown(path, text)?;
    // A rewrite must reproduce the unknown keys, so a file whose unknown part
    // cannot be written back is refused here instead of when it is rewritten.
    config.render().map_err(|_error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: "the unknown keys cannot be written back to a valid file".to_owned(),
    })?;
    Ok(config)
}

/// Parses the current or an exact previous schema at the installer upgrade boundary.
///
/// A previous schema gets the documented defaults for the tables it lacks
/// and keeps every value it records.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per previous schema, each spelling its exact shape"
)]
fn parse_upgrade_source(path: &Path, text: &str) -> Result<(ServiceConfig, u32), ConfigError> {
    let parse_error = |error: toml::de::Error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: diagnostic(text, &error),
    };
    let table: toml::Table = toml::from_str(text).map_err(parse_error)?;
    match table.get("schema_version") {
        Some(toml::Value::Integer(2)) => {
            let previous: RawConfigV2 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: RawInput {
                    initial_startup_grace_ms: millis(DEFAULT_INPUT_TIMING.initial_startup_grace),
                    submit_delay_ms: millis(DEFAULT_INPUT_TIMING.submit_delay),
                    write_stall_timeout_ms: millis(DEFAULT_INPUT_TIMING.write_stall_timeout),
                },
                waits: RawWaits::from(DEFAULT_WAIT_TIMING),
                hooks: RawHooks::DEFAULT,
                events: RawEvents::DEFAULT,
                inventory: RawInventory::default(),
                worker_journals: RawWorkerJournals::default(),
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 2);
            refuse_unknown_keys(path, text, 2)?;
            Ok((config_from_raw(raw)?, 2))
        }
        Some(toml::Value::Integer(4)) => {
            let previous: RawConfigV4 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: RawInput {
                    initial_startup_grace_ms: previous.input.initial_startup_grace_ms,
                    submit_delay_ms: previous.input.submit_delay_ms,
                    write_stall_timeout_ms: millis(DEFAULT_INPUT_TIMING.write_stall_timeout),
                },
                waits: previous.waits,
                hooks: RawHooks::DEFAULT,
                events: RawEvents::DEFAULT,
                inventory: RawInventory::default(),
                worker_journals: RawWorkerJournals::default(),
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 4);
            refuse_unknown_keys(path, text, 4)?;
            Ok((config_from_raw(raw)?, 4))
        }
        Some(toml::Value::Integer(5)) => {
            let previous: RawConfigV5 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: previous.input,
                waits: previous.waits,
                hooks: RawHooks::DEFAULT,
                events: RawEvents::DEFAULT,
                inventory: RawInventory::default(),
                worker_journals: RawWorkerJournals::default(),
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 5);
            refuse_unknown_keys(path, text, 5)?;
            Ok((config_from_raw(raw)?, 5))
        }
        Some(toml::Value::Integer(6)) => {
            let previous: RawConfigV6 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: previous.input,
                waits: previous.waits,
                hooks: previous.hooks,
                events: RawEvents::DEFAULT,
                inventory: RawInventory::default(),
                worker_journals: RawWorkerJournals::default(),
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 6);
            refuse_unknown_keys(path, text, 6)?;
            Ok((config_from_raw(raw)?, 6))
        }
        Some(toml::Value::Integer(3)) => {
            let previous: RawConfigV3 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: RawInput {
                    initial_startup_grace_ms: previous.input.initial_startup_grace_ms,
                    submit_delay_ms: previous.input.submit_delay_ms,
                    write_stall_timeout_ms: millis(DEFAULT_INPUT_TIMING.write_stall_timeout),
                },
                waits: RawWaits::from(DEFAULT_WAIT_TIMING),
                hooks: RawHooks::DEFAULT,
                events: RawEvents::DEFAULT,
                inventory: RawInventory::default(),
                worker_journals: RawWorkerJournals::default(),
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 3);
            refuse_unknown_keys(path, text, 3)?;
            Ok((config_from_raw(raw)?, 3))
        }
        _ => Ok((parse(path, text)?, SCHEMA_VERSION)),
    }
}

/// Fails when a previous-schema file holds a key outside its exact shape.
///
/// The top level and the version-specific tables are rejected by their strict
/// raw types; this covers the tables the previous schemas share with the
/// current one, which ignore unknown keys.
fn refuse_unknown_keys(path: &Path, text: &str, schema: u32) -> Result<(), ConfigError> {
    match scan_unknown(path, text)?.keys.first() {
        None => Ok(()),
        Some(key) => Err(ConfigError::Parse {
            path: path.to_path_buf(),
            message: format!(
                "unknown field `{}` (line {}); a schema {schema} file is read in its exact shape",
                key.path, key.line
            ),
        }),
    }
}

impl RawConfig {
    /// A value of every key the current schema writes, used only to derive
    /// the set of known keys.
    ///
    /// A key that is omitted from the rendered file while it equals its
    /// default must still serialize here, or it would be reported as unknown.
    fn known_keys_sample() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            prefix: String::new(),
            active_version: String::new(),
            namespace: RawNamespace {
                uid: 0,
                state_root: String::new(),
                runtime_root: String::new(),
            },
            deadlines: RawDeadlines {
                worker_connect_ms: 0,
                worker_initialize_ms: 0,
                launchctl_command_ms: 0,
                worker_exit_timeout_ms: 0,
                daemon_exit_timeout_ms: 0,
                daemon_restart_throttle_ms: 0,
            },
            environment: RawEnvironment {
                search_path: Vec::new(),
                allowlist: Vec::new(),
            },
            sweep: RawSweep { grace_ms: 0 },
            input: RawInput {
                initial_startup_grace_ms: 0,
                submit_delay_ms: 0,
                write_stall_timeout_ms: 0,
            },
            waits: RawWaits {
                turn_default_ms: 0,
                turn_max_ms: 0,
                keepalive_idle_ms: default_keepalive_idle_ms() + 1,
                keepalive_interval_ms: default_keepalive_interval_ms() + 1,
                keepalive_count: default_keepalive_count() + 1,
            },
            hooks: RawHooks {
                stderr_tail_bytes: DEFAULT_HOOK_STDERR_TAIL_BYTES,
                required: vec![String::new()],
            },
            events: RawEvents::DEFAULT,
            inventory: RawInventory::known_keys_sample(),
            worker_journals: RawWorkerJournals::known_keys_sample(),
            limits: RawLimits { open_files: 0 },
        }
    }
}

/// Finds every key or table of `text` that the current schema does not know.
///
/// The known set is the serialized shape of [`RawConfig`], so it cannot drift
/// from the typed parse. The text has already parsed as the current schema, so
/// the strict tables hold no unknown key here.
fn scan_unknown(path: &Path, text: &str) -> Result<UnknownKeys, ConfigError> {
    let document = ImDocument::parse(text).map_err(|_error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: "TOML parse error".to_owned(),
    })?;
    let shape = toml::Table::try_from(RawConfig::known_keys_sample())
        .expect("the current shape always serializes as a TOML table");
    let mut scan = UnknownScan {
        text,
        keys: Vec::new(),
        kept: Vec::new(),
    };
    scan.walk(document.as_table(), Some(&shape), &mut Vec::new(), 1);
    Ok(UnknownKeys {
        keys: scan.keys,
        kept: scan.kept,
    })
}

/// State of one [`scan_unknown`] walk over a parsed document.
struct UnknownScan<'a> {
    text: &'a str,
    keys: Vec<UnknownKey>,
    kept: Vec<KeptItem>,
}

impl UnknownScan<'_> {
    /// Visits `table`. `shape` is the known counterpart, or `None` inside an
    /// unknown subtree, where every key is unknown and is only reported.
    fn walk(
        &mut self,
        table: &dyn TableLike,
        shape: Option<&toml::Table>,
        parents: &mut Vec<String>,
        parent_line: usize,
    ) {
        for (name, item) in table.iter() {
            let line = table
                .get_key_value(name)
                .and_then(|(key, _item)| key.span())
                .or_else(|| item.span())
                .map_or(parent_line, |span| position(self.text, span.start).0);
            let known = shape.and_then(|shape| shape.get(name));
            let reported = display_key(name);
            match known {
                Some(toml::Value::Table(known_table)) => {
                    if let Some(child) = item.as_table_like() {
                        parents.push(name.to_owned());
                        self.walk(child, Some(known_table), parents, line);
                        parents.pop();
                    }
                }
                Some(_leaf) => {}
                None => {
                    let path = join_path(parents, &reported);
                    let suggestion = shape
                        .and_then(|shape| closest_known_key(shape, name))
                        .map(|known| join_path(parents, known));
                    self.keys.push(UnknownKey {
                        path,
                        line,
                        suggestion,
                    });
                    if shape.is_some() {
                        self.keep(parents, name, item);
                    }
                    if let Some(child) = item.as_table_like() {
                        parents.push(reported);
                        self.walk(child, None, parents, line);
                        parents.pop();
                    }
                }
            }
        }
    }

    /// Records `item` as found in the known table at `parents`.
    fn keep(&mut self, parents: &[String], name: &str, item: &Item) {
        self.kept.push(KeptItem {
            parents: parents.to_vec(),
            name: name.to_owned(),
            item: item.clone(),
        });
    }
}

/// Joins a parent path and a key name with a dot.
fn join_path(parents: &[String], name: &str) -> String {
    let mut path = parents.join(".");
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(name);
    path
}

/// Escapes control characters of a key name and bounds its length, so a
/// hostile key cannot inject terminal escapes or flood a log line.
fn display_key(name: &str) -> String {
    let escaped: String = name.escape_debug().collect();
    if escaped.len() <= MAX_REPORTED_PATH_BYTES {
        return escaped;
    }
    let mut end = MAX_REPORTED_PATH_BYTES;
    while !escaped.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &escaped[..end])
}

/// Returns the known sibling key closest to the unknown `name`, if any is
/// within [`MAX_SUGGESTION_DISTANCE`].
fn closest_known_key<'a>(shape: &'a toml::Table, name: &str) -> Option<&'a str> {
    let length = name.chars().count();
    shape
        .keys()
        .filter_map(|known| {
            let distance = edit_distance(known, name)?;
            (distance < length).then_some((distance, known.as_str()))
        })
        .min_by_key(|(distance, _known)| *distance)
        .map(|(_distance, known)| known)
}

/// Levenshtein distance between two names, or `None` above
/// [`MAX_SUGGESTION_DISTANCE`].
fn edit_distance(known: &str, name: &str) -> Option<usize> {
    let known: Vec<char> = known.chars().collect();
    let name: Vec<char> = name.chars().collect();
    if known.len().abs_diff(name.len()) > MAX_SUGGESTION_DISTANCE {
        return None;
    }
    let mut previous: Vec<usize> = (0..=name.len()).collect();
    for (row, known_char) in known.iter().enumerate() {
        let mut current = vec![row + 1];
        for (column, name_char) in name.iter().enumerate() {
            let substitution = previous[column] + usize::from(known_char != name_char);
            current.push(
                substitution
                    .min(previous[column + 1] + 1)
                    .min(current[column] + 1),
            );
        }
        previous = current;
    }
    let distance = previous[name.len()];
    (distance <= MAX_SUGGESTION_DISTANCE).then_some(distance)
}

/// Merges the kept unknown items into the rendered typed document.
///
/// Every known table exists in the typed rendering, so each item goes into an
/// existing table. Kept tables are placed after every typed table, in their
/// original order. The result is parsed again, so a merge that would produce
/// an invalid document is an error instead of a bad file.
fn merge_kept_keys(typed: &str, kept: &[KeptItem]) -> Result<String, KeptMergeError> {
    let mut document: DocumentMut = typed.parse().map_err(|_error| KeptMergeError)?;
    let base = max_position(document.as_table()).map_or(0, |last| last + 1);
    for entry in kept {
        let mut table = document.as_table_mut();
        for parent in &entry.parents {
            table = table
                .get_mut(parent)
                .and_then(Item::as_table_mut)
                .ok_or(KeptMergeError)?;
        }
        if table.contains_key(&entry.name) {
            return Err(KeptMergeError);
        }
        let mut item = entry.item.clone();
        shift_positions(&mut item, base);
        table.insert(&entry.name, item);
    }
    let merged = document.to_string();
    merged
        .parse::<DocumentMut>()
        .map_err(|_error| KeptMergeError)?;
    Ok(merged)
}

/// Largest document position among the tables below `table`.
fn max_position(table: &Table) -> Option<usize> {
    table
        .iter()
        .filter_map(|(_name, item)| match item {
            Item::Table(child) => max_position(child).max(child.position()),
            _other => None,
        })
        .max()
}

/// Moves every table position below `item` by `base`, keeping their order.
fn shift_positions(item: &mut Item, base: usize) {
    match item {
        Item::Table(table) => {
            if let Some(position) = table.position() {
                table.set_position(base + position);
            }
            for (_name, child) in table.iter_mut() {
                shift_positions(child, base);
            }
        }
        Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                if let Some(position) = table.position() {
                    table.set_position(base + position);
                }
                for (_name, child) in table.iter_mut() {
                    shift_positions(child, base);
                }
            }
        }
        Item::Value(_) | Item::None => {}
    }
}

/// Like [`diagnostic`], and names the unknown key a missing required key was
/// probably meant to be.
fn diagnostic_with_hint(path: &Path, text: &str, error: &toml::de::Error) -> String {
    let mut message = diagnostic(text, error);
    let missing = error
        .message()
        .strip_prefix("missing field `")
        .and_then(|rest| rest.strip_suffix('`'));
    if let (Some(missing), Ok(unknown)) = (missing, scan_unknown(path, text)) {
        let hinted = unknown.keys.iter().find_map(|key| {
            let known = key.suggestion.as_deref()?;
            (known == missing || known.ends_with(&format!(".{missing}")))
                .then_some((known, key.path.as_str()))
        });
        if let Some((known, unknown_path)) = hinted {
            std::fmt::Write::write_fmt(
                &mut message,
                format_args!("; did you mean `{known}`? `{unknown_path}` is not a key"),
            )
            .expect("writing to String is infallible");
        }
    }
    message
}

/// Converts the parsed current shape into validated typed values.
fn config_from_raw(raw: RawConfig) -> Result<ServiceConfig, ConfigError> {
    let search_path = SearchPath::new(
        raw.environment
            .search_path
            .into_iter()
            .map(PathBuf::from)
            .collect(),
    )
    .map_err(|source| ConfigError::InvalidSearchPath {
        detail: source.to_string(),
    })?;
    let max_age_days = raw.events.max_age_days;
    if max_age_days > MAX_EVENT_AGE_DAYS {
        return Err(ConfigError::OutOfRange {
            key: "events.max_age_days",
            value: max_age_days,
            min: MIN_EVENT_AGE_DAYS,
            max: MAX_EVENT_AGE_DAYS,
        });
    }
    let journal_age_days = raw.worker_journals.max_age_days;
    if journal_age_days > MAX_WORKER_JOURNAL_AGE_DAYS {
        return Err(ConfigError::OutOfRange {
            key: "worker_journals.max_age_days",
            value: journal_age_days,
            min: MIN_WORKER_JOURNAL_AGE_DAYS,
            max: MAX_WORKER_JOURNAL_AGE_DAYS,
        });
    }
    ServiceConfig::new(ConfigSpec {
        prefix: PathBuf::from(raw.prefix),
        active_version: raw.active_version,
        uid: raw.namespace.uid,
        state_root: PathBuf::from(raw.namespace.state_root),
        runtime_root: PathBuf::from(raw.namespace.runtime_root),
        deadlines: Deadlines {
            worker_connect: Duration::from_millis(raw.deadlines.worker_connect_ms),
            worker_initialize: Duration::from_millis(raw.deadlines.worker_initialize_ms),
            launchctl_command: Duration::from_millis(raw.deadlines.launchctl_command_ms),
            worker_exit_timeout: Duration::from_millis(raw.deadlines.worker_exit_timeout_ms),
            daemon_exit_timeout: Duration::from_millis(raw.deadlines.daemon_exit_timeout_ms),
            daemon_restart_throttle: Duration::from_millis(
                raw.deadlines.daemon_restart_throttle_ms,
            ),
        },
        environment_allowlist: raw.environment.allowlist,
        search_path,
        sweep_grace: Duration::from_millis(raw.sweep.grace_ms),
        input_timing: InputTiming {
            initial_startup_grace: Duration::from_millis(raw.input.initial_startup_grace_ms),
            submit_delay: Duration::from_millis(raw.input.submit_delay_ms),
            write_stall_timeout: Duration::from_millis(raw.input.write_stall_timeout_ms),
        },
        wait_timing: WaitTiming {
            turn_default: Duration::from_millis(raw.waits.turn_default_ms),
            turn_max: Duration::from_millis(raw.waits.turn_max_ms),
            keepalive: KeepaliveTiming {
                idle: Duration::from_millis(raw.waits.keepalive_idle_ms),
                interval: Duration::from_millis(raw.waits.keepalive_interval_ms),
                count: raw.waits.keepalive_count,
            },
        },
        hook_stderr_tail_bytes: raw.hooks.stderr_tail_bytes,
        hook_required: parse_required_hooks(&raw.hooks.required)?,
        event_retention: EventRetention {
            max_file_bytes: raw.events.max_file_bytes,
            max_files: raw.events.max_files,
            max_age: Duration::from_secs(max_age_days * SECONDS_PER_DAY),
        },
        inventory_probe: InventoryProbe {
            cache_ttl: Duration::from_millis(raw.inventory.probe_cache_ttl_ms),
            failure_ttl: Duration::from_millis(raw.inventory.probe_failure_ttl_ms),
            cache_max_entries: raw.inventory.probe_cache_max_entries,
            parallelism: raw.inventory.probe_parallelism,
        },
        worker_journal_max_age: Duration::from_secs(journal_age_days * SECONDS_PER_DAY),
        open_files: raw.limits.open_files,
    })
}

/// Sanitized diagnostic for one TOML parse failure over `text`.
///
/// The parser's `Display` renders the source line the error span points at,
/// which would print arbitrary file contents — including a hand-edited line
/// such as an unknown `token = "…"` key — into sticky destinations: journald,
/// the CLI error envelope, and operator logs. Only the parser's message text
/// and a byte-offset-derived position survive, never source text: argument
/// values quoted in parser messages are replaced by a fixed placeholder, and
/// the line and column are computed from the error's span.
fn diagnostic(text: &str, error: &toml::de::Error) -> String {
    let mut message = redact_quoted_values(error.message());
    if message.is_empty() {
        message.push_str("TOML parse error");
    }
    if let Some(span) = error.span() {
        let (line, column) = position(text, span.start);
        std::fmt::Write::write_fmt(
            &mut message,
            format_args!(" (line {line}, column {column})"),
        )
        .expect("writing to String is infallible");
    }
    message
}

/// Replaces the content of every `"`-delimited run with a fixed placeholder.
///
/// Parser messages double-quote argument values only; schema identifiers,
/// key names, and error descriptors use backticks or plain words. A `\`
/// escapes exactly one following character, so an escaped `\"` never closes
/// a run that its `Debug`-escaped token did open. An unterminated run also
/// renders as the placeholder: replaying that tail could print file text.
fn redact_quoted_values(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len());
    let mut inside_value = false;
    let mut escaped = false;
    for character in message.chars() {
        if inside_value {
            if escaped {
                escaped = false;
            } else if character == '"' {
                inside_value = false;
                redacted.push('"');
                redacted.push_str(REDACTED_PLACEHOLDER);
                redacted.push('"');
            } else if character == '\\' {
                escaped = true;
            }
            continue;
        }
        if character == '"' {
            inside_value = true;
        } else {
            redacted.push(character);
        }
    }
    if inside_value {
        // Fail closed: what followed the open quote may be file text.
        redacted.push('"');
        redacted.push_str(REDACTED_PLACEHOLDER);
        redacted.push('"');
    }
    redacted
}

/// Placeholder shown for parser-quoted file contents.
const REDACTED_PLACEHOLDER: &str = "<redacted>";

/// Returns the 1-based line and character column of a byte offset in `text`.
///
/// A non-character-boundary offset backs up to the nearest boundary, so an
/// offset into a multi-byte character reports the line the character starts on.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut line = 1;
    let mut column = 1;
    for (index, character) in text.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Splits an absolute normalized configuration path into directory and name.
fn split_config_path(path: &Path) -> Result<(&Path, &OsStr), ConfigError> {
    let invalid = || ConfigError::ConfigPath {
        path: path.to_path_buf(),
    };
    if !is_normalized_absolute(path) {
        return Err(invalid());
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, name)),
        _ => Err(invalid()),
    }
}

/// Opens an existing owner-safe directory, or creates a missing one as `0700`.
///
/// Creation also validates the result, so a directory another process
/// creates in between with a different mode is rejected, not adopted.
fn open_or_create_directory(parent: &Path) -> Result<TrustedDir, FsError> {
    match TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS) {
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            TrustedDir::open_or_create_absolute(parent, DIRECTORY_MODE)
        }
        result => result,
    }
}

/// Maps a trusted-filesystem failure to the matching configuration error.
fn classify_read_error(path: &Path, source: FsError) -> ConfigError {
    let path = path.to_path_buf();
    match source {
        FsError::FileTooLarge { max_bytes, .. } => ConfigError::TooLarge { path, max_bytes },
        FsError::UnsafeType { .. }
        | FsError::UnsafeOwner { .. }
        | FsError::UnsafeMode { .. }
        | FsError::UnsafeAcl { .. }
        | FsError::UnsafeLinkCount { .. } => ConfigError::Untrusted { path, source },
        source => ConfigError::Io { path, source },
    }
}

/// Validates an installation prefix exactly as [`ServiceConfig::new`] does.
///
/// Callers that act on a prefix before building the configuration check it
/// here first, so a prefix the configuration would reject is refused before
/// any of their effects.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidPath`] with key `prefix` for a relative,
/// unnormalized, non-UTF-8, or oversized path.
pub fn validate_prefix(prefix: &Path) -> Result<InstallLayout, ConfigError> {
    validate_path("prefix", prefix)?;
    InstallLayout::new(prefix).map_err(|_invalid_prefix| ConfigError::InvalidPath {
        key: "prefix",
        length: prefix.as_os_str().len(),
        reason: "is not a usable installation prefix",
        max_bytes: MAX_PATH_BYTES,
    })
}

/// Validates one recorded path key.
fn validate_path(key: &'static str, path: &Path) -> Result<(), ConfigError> {
    match path_error(path) {
        None => Ok(()),
        Some(reason) => Err(ConfigError::InvalidPath {
            key,
            length: path.as_os_str().len(),
            reason,
            max_bytes: MAX_PATH_BYTES,
        }),
    }
}

/// Explains why `path` is not an absolute normalized UTF-8 path within [`MAX_PATH_BYTES`].
fn path_error(path: &Path) -> Option<&'static str> {
    if path.as_os_str().len() > MAX_PATH_BYTES {
        Some("is too long")
    } else if path.to_str().is_none() {
        Some("is not UTF-8")
    } else if !path.is_absolute() {
        Some("is not absolute")
    } else if path.as_os_str().as_encoded_bytes().contains(&0) {
        Some("contains a NUL byte")
    } else if !is_normalized_absolute(path) {
        Some("is not normalized (`.`, `..`, or a repeated or trailing `/`)")
    } else {
        None
    }
}

/// Validates one recorded duration key.
fn validate_duration(key: &'static str, value: Duration) -> Result<(), ConfigError> {
    if !value.subsec_nanos().is_multiple_of(NANOS_PER_MILLI) {
        return Err(ConfigError::Precision { key });
    }
    if value.is_zero() || value > MAX_DEADLINE {
        return Err(ConfigError::OutOfRange {
            key,
            value: u64::try_from(value.as_millis()).unwrap_or(u64::MAX),
            min: 1,
            max: millis(MAX_DEADLINE),
        });
    }
    Ok(())
}

/// Validates each `[inventory]` bound.
fn validate_inventory_probe(probe: InventoryProbe) -> Result<(), ConfigError> {
    validate_duration("inventory.probe_cache_ttl_ms", probe.cache_ttl)?;
    validate_duration("inventory.probe_failure_ttl_ms", probe.failure_ttl)?;
    if !(MIN_PROBE_CACHE_ENTRIES..=MAX_PROBE_CACHE_ENTRIES).contains(&probe.cache_max_entries) {
        return Err(ConfigError::OutOfRange {
            key: "inventory.probe_cache_max_entries",
            value: u64::from(probe.cache_max_entries),
            min: u64::from(MIN_PROBE_CACHE_ENTRIES),
            max: u64::from(MAX_PROBE_CACHE_ENTRIES),
        });
    }
    if !(MIN_PROBE_PARALLELISM..=MAX_PROBE_PARALLELISM).contains(&probe.parallelism) {
        return Err(ConfigError::OutOfRange {
            key: "inventory.probe_parallelism",
            value: u64::from(probe.parallelism),
            min: u64::from(MIN_PROBE_PARALLELISM),
            max: u64::from(MAX_PROBE_PARALLELISM),
        });
    }
    Ok(())
}

/// Validates the `[worker_journals]` bound.
fn validate_worker_journal_max_age(max_age: Duration) -> Result<(), ConfigError> {
    if !max_age.as_secs().is_multiple_of(SECONDS_PER_DAY) || max_age.subsec_nanos() != 0 {
        return Err(ConfigError::Precision {
            key: "worker_journals.max_age_days",
        });
    }
    let days = max_age.as_secs() / SECONDS_PER_DAY;
    if !(MIN_WORKER_JOURNAL_AGE_DAYS..=MAX_WORKER_JOURNAL_AGE_DAYS).contains(&days) {
        return Err(ConfigError::OutOfRange {
            key: "worker_journals.max_age_days",
            value: days,
            min: MIN_WORKER_JOURNAL_AGE_DAYS,
            max: MAX_WORKER_JOURNAL_AGE_DAYS,
        });
    }
    Ok(())
}

/// Validates each `[events]` bound.
fn validate_event_retention(retention: EventRetention) -> Result<(), ConfigError> {
    if !(MIN_EVENT_FILE_BYTES..=MAX_EVENT_FILE_BYTES).contains(&retention.max_file_bytes) {
        return Err(ConfigError::OutOfRange {
            key: "events.max_file_bytes",
            value: retention.max_file_bytes,
            min: MIN_EVENT_FILE_BYTES,
            max: MAX_EVENT_FILE_BYTES,
        });
    }
    if !(MIN_EVENT_FILES..=MAX_EVENT_FILES).contains(&retention.max_files) {
        return Err(ConfigError::OutOfRange {
            key: "events.max_files",
            value: u64::from(retention.max_files),
            min: u64::from(MIN_EVENT_FILES),
            max: u64::from(MAX_EVENT_FILES),
        });
    }
    if !retention.max_age.as_secs().is_multiple_of(SECONDS_PER_DAY)
        || retention.max_age.subsec_nanos() != 0
    {
        return Err(ConfigError::Precision {
            key: "events.max_age_days",
        });
    }
    let days = retention.max_age.as_secs() / SECONDS_PER_DAY;
    if !(MIN_EVENT_AGE_DAYS..=MAX_EVENT_AGE_DAYS).contains(&days) {
        return Err(ConfigError::OutOfRange {
            key: "events.max_age_days",
            value: days,
            min: MIN_EVENT_AGE_DAYS,
            max: MAX_EVENT_AGE_DAYS,
        });
    }
    Ok(())
}

/// Validates each `[waits]` bound and that the default stays within the ceiling.
fn validate_wait_timing(timing: WaitTiming) -> Result<(), ConfigError> {
    for (key, value) in [
        ("waits.turn_default_ms", timing.turn_default),
        ("waits.turn_max_ms", timing.turn_max),
    ] {
        if !value.subsec_nanos().is_multiple_of(NANOS_PER_MILLI) {
            return Err(ConfigError::Precision { key });
        }
        if !(MIN_TURN_WAIT..=MAX_TURN_WAIT).contains(&value) {
            return Err(ConfigError::OutOfRange {
                key,
                value: u64::try_from(value.as_millis()).unwrap_or(u64::MAX),
                min: millis(MIN_TURN_WAIT),
                max: millis(MAX_TURN_WAIT),
            });
        }
    }
    if timing.turn_default > timing.turn_max {
        return Err(ConfigError::WaitDefaultAboveMax {
            default_ms: millis(timing.turn_default),
            max_ms: millis(timing.turn_max),
        });
    }
    validate_keepalive_timing(timing.keepalive)
}

/// Validates the `[waits]` keepalive keys against the kernel's whole-second
/// granularity and the documented bounds.
fn validate_keepalive_timing(keepalive: KeepaliveTiming) -> Result<(), ConfigError> {
    for (key, value) in [
        ("waits.keepalive_idle_ms", keepalive.idle),
        ("waits.keepalive_interval_ms", keepalive.interval),
    ] {
        if !value.subsec_nanos().is_multiple_of(NANOS_PER_MILLI) {
            return Err(ConfigError::Precision { key });
        }
        if !(MIN_KEEPALIVE_TIME..=MAX_KEEPALIVE_TIME).contains(&value) {
            return Err(ConfigError::OutOfRange {
                key,
                value: u64::try_from(value.as_millis()).unwrap_or(u64::MAX),
                min: millis(MIN_KEEPALIVE_TIME),
                max: millis(MAX_KEEPALIVE_TIME),
            });
        }
        if value.subsec_millis() != 0 {
            return Err(ConfigError::WholeSeconds { key });
        }
    }
    if !(MIN_KEEPALIVE_COUNT..=MAX_KEEPALIVE_COUNT).contains(&keepalive.count) {
        return Err(ConfigError::OutOfRange {
            key: "waits.keepalive_count",
            value: u64::from(keepalive.count),
            min: u64::from(MIN_KEEPALIVE_COUNT),
            max: u64::from(MAX_KEEPALIVE_COUNT),
        });
    }
    Ok(())
}

/// Resolves the `hooks.required` entries to the events a release can require.
///
/// An entry that is no hook event fails without being echoed. A known event
/// this release cannot require names itself in the error.
fn parse_required_hooks(entries: &[String]) -> Result<Vec<RequiredHook>, ConfigError> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| match entry.as_str() {
            "post-create" => Ok(RequiredHook::PostCreate),
            other => match HOOK_EVENT_NAMES.iter().find(|name| **name == other) {
                Some(event) => Err(ConfigError::UnsupportedRequiredHook { index, event }),
                None => Err(ConfigError::InvalidHookEvent { index }),
            },
        })
        .collect()
}

/// Rejects an event listed twice in `hooks.required`.
fn validate_required_hooks(hooks: &[RequiredHook]) -> Result<(), ConfigError> {
    for (index, hook) in hooks.iter().enumerate() {
        if let Some(first) = hooks.iter().position(|earlier| earlier == hook) {
            if first != index {
                return Err(ConfigError::DuplicateRequiredHook { index, first });
            }
        }
    }
    Ok(())
}

/// Validates the environment allowlist syntax, size, and uniqueness.
fn validate_allowlist(patterns: &[String]) -> Result<(), ConfigError> {
    if patterns.is_empty() || patterns.len() > MAX_ALLOWLIST_ENTRIES {
        return Err(ConfigError::AllowlistSize {
            count: patterns.len(),
            max: MAX_ALLOWLIST_ENTRIES,
        });
    }
    let mut seen = BTreeMap::new();
    for (index, pattern) in patterns.iter().enumerate() {
        if let Some(reason) = pattern_error(pattern) {
            return Err(ConfigError::InvalidPattern {
                index,
                length: pattern.len(),
                reason,
            });
        }
        if let Some(&first) = seen.get(pattern.as_str()) {
            return Err(ConfigError::DuplicatePattern { index, first });
        }
        seen.insert(pattern.as_str(), index);
    }
    Ok(())
}

/// Explains why `pattern` is not `[A-Za-z_][A-Za-z0-9_]*` with an optional trailing `*`.
fn pattern_error(pattern: &str) -> Option<&'static str> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Some("longer than 128 bytes");
    }
    let name = pattern.strip_suffix('*').unwrap_or(pattern);
    let mut bytes = name.bytes();
    match bytes.next() {
        None => Some("a pattern needs at least one name character before any `*`"),
        Some(first) if !(first.is_ascii_alphabetic() || first == b'_') => {
            Some("a name must start with an ASCII letter or `_`")
        }
        Some(_) if !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') => Some(
            "a name may contain only ASCII letters, digits, and `_`, with `*` only as the last character",
        ),
        Some(_) => None,
    }
}

/// Compares a recorded root with the canonical form of the running root.
fn verify_root(key: &'static str, recorded: &Path, actual: &Path) -> Result<(), ConfigError> {
    let canonical = std::fs::canonicalize(actual).map_err(|source| ConfigError::Canonicalize {
        key,
        path: actual.to_path_buf(),
        source,
    })?;
    if canonical == recorded {
        Ok(())
    } else {
        Err(ConfigError::NamespaceMismatch {
            key,
            recorded: recorded.display().to_string(),
            actual: canonical.display().to_string(),
        })
    }
}

/// Returns a validated duration in whole milliseconds.
///
/// # Panics
///
/// Panics above `u64::MAX` milliseconds, which no validated duration reaches:
/// the longest is [`MAX_TURN_WAIT`].
fn millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).expect("validated durations are at most MAX_TURN_WAIT")
}

/// Returns a validated path as UTF-8.
fn utf8(path: &Path) -> &str {
    path.to_str()
        .expect("recorded paths were validated as UTF-8 by ServiceConfig::new")
}
