//! Owner limits of workflow plugin runs, read from `<config>/pohunek/workflow.toml`.
//!
//! The file is optional: when it or its directory is absent, every knob has
//! its documented default. The directory, when it exists, must be one no
//! other account can write, whether or not the file is there, because another
//! account could otherwise plant the file. When the file exists it must be a
//! regular file owned by the user with mode `0600`, and
//! parsing is strict: unknown tables or keys, wrong types and out-of-range
//! values fail with [`Error::ConfigInvalid`] before anything runs. Tables of
//! later workflow slices are unknown keys here.
//!
//! `[jobs]` and `[services]` apply to the supervised runs of the plugin units
//! (see [`super::supervised`]). Every knob is read again at each run, and a
//! job outside `jobs.min_interval_s` or `jobs.timeout_max_ms` is refused then
//! as when it was enabled, except `jobs.check_period_s`, which is written
//! into the job's timer unit when the plugin is enabled and so needs a new
//! `pohunek workflow enable` to change.

// Rust guideline compliant 2026-10-13

use std::io::ErrorKind;
use std::path::Path;

use std::time::Duration;

use pohunek_platform::filesystem::{FsError, TrustedDir};
use pohunek_platform::supervisor::{MAX_JOB_INTERVAL, MAX_JOB_TIMEOUT};
use serde::Deserialize;

use super::Error;

/// File name of the knobs file in pohunek's config directory.
pub(crate) const FILE_NAME: &str = "workflow.toml";

/// Largest accepted knobs file, checked before parsing.
///
/// The same ceiling as a plugin manifest; the file holds a few integers.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Required mode of the knobs file: owner read and write only, like
/// `service.toml`, because the file sets safety limits.
const FILE_MODE: u32 = 0o600;

/// Directory mode bits that would let another account replace the file.
const FORBIDDEN_DIRECTORY_BITS: u32 = 0o022;

/// Hard ceiling of `limits.max_context_bytes`: 96 KiB.
///
/// The context travels as one environment string, and Linux refuses a
/// string longer than `MAX_ARG_STRLEN` (128 KiB) at `exec`; the margin keeps
/// the variable name and the rest of the environment below that bound.
pub(crate) const MAX_CONTEXT_BYTES: u64 = 96 * 1024;

/// Ceiling of any action timeout: one day. An interactive run that needs
/// longer is a hang, not a workflow step.
const MAX_ACTION_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Ceiling of `limits.kill_grace_ms`: one minute between `SIGTERM` and
/// `SIGKILL` is ample for a plugin to clean up.
pub(crate) const MAX_KILL_GRACE_MS: u64 = 60 * 1000;

/// Time a supervised run needs besides the plugin's own deadline and the kill
/// grace: verifying the installed files, taking the lock and writing records.
///
/// A unit's native start timeout is [`MAX_JOB_TIMEOUT`]; keeping every job
/// deadline at least this margin plus [`MAX_KILL_GRACE_MS`] below it lets the
/// supervised run record `timeout` before the service manager kills it.
/// Lowering it risks the manager's kill on a slow host with a large plugin.
pub(crate) const SUPERVISED_MARGIN_MS: u64 = 30 * 1000;

/// Ceiling of `jobs.timeout_max_ms`: the platform's longest unit start
/// timeout less the longest kill grace and [`SUPERVISED_MARGIN_MS`].
pub(crate) const MAX_JOB_TIMEOUT_MS: u64 =
    MAX_JOB_TIMEOUT.as_secs() * 1000 - MAX_KILL_GRACE_MS - SUPERVISED_MARGIN_MS;

/// Ceiling of `jobs.min_interval_s`: the longest interval the service
/// managers accept for a periodic unit, which is also the longest `every_s`
/// a manifest may declare.
const MAX_INTERVAL_S: u64 = MAX_JOB_INTERVAL.as_secs();

/// Ceiling of `jobs.check_period_s`: one hour. The check period is the
/// timer's own interval, counted in awake time and restarted at each login,
/// so a longer one would let a due job wait hours after a resume.
const MAX_CHECK_PERIOD_S: u64 = 60 * 60;

/// Ceiling of the service backoff knobs: one day. A service restarted less
/// often than daily is effectively stopped, which `workflow disable` says
/// openly.
const MAX_SERVICE_DELAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Ceiling of the per-stream output caps: 1 GiB. A captured stream is held
/// in memory, so the cap must stay addressable on every host.
const MAX_STREAM_BYTES: u64 = 1024 * 1024 * 1024;

/// Floor of `logs.run_log_file_bytes`: 4 KiB holds several run records, so a
/// record never exceeds an empty file.
const MIN_RUN_LOG_FILE_BYTES: u64 = 4 * 1024;

/// Ceiling of `logs.run_log_file_bytes`: 64 MiB, twice the daemon log file.
const MAX_RUN_LOG_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Ceiling of `logs.run_log_files`, the active file included.
const MAX_RUN_LOG_FILES: u64 = 64;

/// Default action deadline: one minute covers an interactive step.
const DEFAULT_ACTION_TIMEOUT_MS: u64 = 60 * 1000;

/// Default ceiling a manifest `timeout_ms` may request: ten minutes, the
/// platform's longest supervised job deadline (`MAX_JOB_TIMEOUT`).
const DEFAULT_ACTION_TIMEOUT_MAX_MS: u64 = 10 * 60 * 1000;

/// Default time between `SIGTERM` and `SIGKILL`: five seconds.
const DEFAULT_KILL_GRACE_MS: u64 = 5 * 1000;

/// Default stdout cap: 16 MiB, far above a report or JSON document and far
/// below what floods a terminal for minutes.
const DEFAULT_MAX_STDOUT_BYTES: u64 = 16 * 1024 * 1024;

/// Default stderr cap: 1 MiB of diagnostics.
const DEFAULT_MAX_STDERR_BYTES: u64 = 1024 * 1024;

/// Default context cap: 64 KiB, below [`MAX_CONTEXT_BYTES`].
const DEFAULT_MAX_CONTEXT_BYTES: u64 = 64 * 1024;

/// Default run log file size: 1 MiB holds thousands of run records.
const DEFAULT_RUN_LOG_FILE_BYTES: u64 = 1024 * 1024;

/// Default run log file count, the active file included: 4 MiB per plugin.
const DEFAULT_RUN_LOG_FILES: u64 = 4;

/// Default deadline of a job that declares no `timeout_ms`: one minute, as
/// for an action.
const DEFAULT_JOB_TIMEOUT_MS: u64 = 60 * 1000;

/// Default ceiling a job's `timeout_ms` may request: five minutes, below
/// [`MAX_JOB_TIMEOUT_MS`].
const DEFAULT_JOB_TIMEOUT_MAX_MS: u64 = 5 * 60 * 1000;

/// Default shortest job interval: one minute. A plugin polling more often
/// than that loads the hosts it polls more than a workflow step should.
const DEFAULT_JOB_MIN_INTERVAL_S: u64 = 60;

/// Default check period: one minute, so a due job starts at most about a
/// minute late after a resume or a login.
const DEFAULT_JOB_CHECK_PERIOD_S: u64 = 60;

/// Default first restart delay of a service after a short run: one second.
const DEFAULT_RESTART_INITIAL_MS: u64 = 1000;

/// Default longest restart delay of a service: five minutes.
///
/// Also paces a refused service, which the service manager would otherwise
/// start again every second; see [`super::supervised`].
pub(crate) const DEFAULT_RESTART_MAX_MS: u64 = 5 * 60 * 1000;

/// Default run length after which a service counts as stable and its
/// backoff starts over: one minute.
const DEFAULT_STABLE_AFTER_MS: u64 = 60 * 1000;

/// Run limits, the `[limits]` table.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Limits {
    /// Deadline of an action that declares no `timeout_ms`.
    pub(crate) action_timeout_default_ms: u64,
    /// Largest `timeout_ms` an action may declare; larger ones are refused.
    pub(crate) action_timeout_max_ms: u64,
    /// Time between `SIGTERM` and `SIGKILL` when a run is stopped; zero
    /// sends `SIGKILL` at once.
    pub(crate) kill_grace_ms: u64,
    /// Bytes of stdout a run may write.
    pub(crate) max_stdout_bytes: u64,
    /// Bytes of stderr a run may write.
    pub(crate) max_stderr_bytes: u64,
    /// Bytes of `POHUNEK_PLUGIN_CONTEXT`; a larger context refuses the run.
    pub(crate) max_context_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            action_timeout_default_ms: DEFAULT_ACTION_TIMEOUT_MS,
            action_timeout_max_ms: DEFAULT_ACTION_TIMEOUT_MAX_MS,
            kill_grace_ms: DEFAULT_KILL_GRACE_MS,
            max_stdout_bytes: DEFAULT_MAX_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
        }
    }
}

/// Run log retention, the `[logs]` table.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Logs {
    /// Bytes of one `runs.jsonl` file before it rotates.
    pub(crate) run_log_file_bytes: u64,
    /// Run log files kept per plugin, the active file included.
    pub(crate) run_log_files: u64,
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            run_log_file_bytes: DEFAULT_RUN_LOG_FILE_BYTES,
            run_log_files: DEFAULT_RUN_LOG_FILES,
        }
    }
}

/// Supervised job limits, the `[jobs]` table.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Jobs {
    /// Deadline of a job that declares no `timeout_ms`.
    pub(crate) timeout_default_ms: u64,
    /// Largest `timeout_ms` a job may declare; larger ones are refused.
    pub(crate) timeout_max_ms: u64,
    /// Shortest `every_s` a job may declare; shorter ones are refused.
    pub(crate) min_interval_s: u64,
    /// Interval of a job's native timer, which checks whether a run is due;
    /// a job's `every_s` below it is used instead.
    pub(crate) check_period_s: u64,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            timeout_default_ms: DEFAULT_JOB_TIMEOUT_MS,
            timeout_max_ms: DEFAULT_JOB_TIMEOUT_MAX_MS,
            min_interval_s: DEFAULT_JOB_MIN_INTERVAL_S,
            check_period_s: DEFAULT_JOB_CHECK_PERIOD_S,
        }
    }
}

/// Supervised service restart pacing, the `[services]` table.
#[expect(
    clippy::struct_field_names,
    reason = "the fields are the owner's knob keys, which name their unit"
)]
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Services {
    /// Delay before the first restart after a short run; it doubles with
    /// each further short run.
    pub(crate) restart_initial_ms: u64,
    /// Longest restart delay; also the pause before a refused service exits.
    pub(crate) restart_max_ms: u64,
    /// Run length after which a run counts as stable and the delay starts
    /// over.
    pub(crate) stable_after_ms: u64,
}

impl Default for Services {
    fn default() -> Self {
        Self {
            restart_initial_ms: DEFAULT_RESTART_INITIAL_MS,
            restart_max_ms: DEFAULT_RESTART_MAX_MS,
            stable_after_ms: DEFAULT_STABLE_AFTER_MS,
        }
    }
}

impl Services {
    /// The restart delay after `failures` short runs in a row: none for the
    /// first start, then `restart_initial_ms` doubling up to
    /// `restart_max_ms`.
    pub(crate) fn delay(&self, failures: u32) -> Duration {
        let Some(doublings) = failures.checked_sub(1) else {
            return Duration::ZERO;
        };
        let factor = 1_u64.checked_shl(doublings).unwrap_or(u64::MAX);
        Duration::from_millis(
            self.restart_initial_ms
                .saturating_mul(factor)
                .min(self.restart_max_ms),
        )
    }
}

/// Every owner knob of workflow runs.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Knobs {
    /// The `[limits]` table.
    pub(crate) limits: Limits,
    /// The `[logs]` table.
    pub(crate) logs: Logs,
    /// The `[jobs]` table.
    pub(crate) jobs: Jobs,
    /// The `[services]` table.
    pub(crate) services: Services,
}

/// Reads the knobs file at `path`; an absent file or directory yields the
/// defaults.
///
/// # Errors
///
/// Returns [`Error::ConfigInvalid`] when the file is unsafe, unreadable,
/// too large, not TOML of the schema, or holds an out-of-range value. The
/// message names a line, a key and its allowed range, never file content.
pub(crate) fn load(path: &Path) -> Result<Knobs, Error> {
    let invalid = |reason: String| Error::ConfigInvalid {
        path: path.display().to_string(),
        reason,
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(invalid("the path has no file name".to_owned()));
    };
    let bytes = match TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS)
        .and_then(|dir| dir.read_file(name, FILE_MODE, MAX_FILE_BYTES))
    {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(Knobs::default()),
        Err(error) => return Err(invalid(unsafe_reason(&error))),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_not_utf8| invalid("the file is not UTF-8".to_owned()))?;
    // Parser messages can quote file values, so only the line survives.
    let knobs: Knobs = toml_edit::de::from_str(text).map_err(|error| {
        let line = error
            .span()
            .and_then(|span| text.get(..span.start))
            .map(|prefix| prefix.matches('\n').count() + 1);
        invalid(format!(
            "the file does not match the knobs schema (syntax, unknown key or wrong type){}",
            line.map(|line| format!(" at line {line}"))
                .unwrap_or_default()
        ))
    })?;
    check_ranges(&knobs).map_err(invalid)?;
    Ok(knobs)
}

/// Checks every knob against its documented range.
#[expect(
    clippy::too_many_lines,
    reason = "one flat row per knob keeps every range in one table"
)]
fn check_ranges(knobs: &Knobs) -> Result<(), String> {
    let limits = &knobs.limits;
    let logs = &knobs.logs;
    let jobs = &knobs.jobs;
    let services = &knobs.services;
    let ranges = [
        (
            "limits.action_timeout_max_ms",
            limits.action_timeout_max_ms,
            1,
            MAX_ACTION_TIMEOUT_MS,
        ),
        (
            "limits.action_timeout_default_ms",
            limits.action_timeout_default_ms,
            1,
            limits.action_timeout_max_ms,
        ),
        (
            "limits.kill_grace_ms",
            limits.kill_grace_ms,
            0,
            MAX_KILL_GRACE_MS,
        ),
        (
            "limits.max_stdout_bytes",
            limits.max_stdout_bytes,
            1,
            MAX_STREAM_BYTES,
        ),
        (
            "limits.max_stderr_bytes",
            limits.max_stderr_bytes,
            1,
            MAX_STREAM_BYTES,
        ),
        (
            "limits.max_context_bytes",
            limits.max_context_bytes,
            1,
            MAX_CONTEXT_BYTES,
        ),
        (
            "logs.run_log_file_bytes",
            logs.run_log_file_bytes,
            MIN_RUN_LOG_FILE_BYTES,
            MAX_RUN_LOG_FILE_BYTES,
        ),
        (
            "logs.run_log_files",
            logs.run_log_files,
            1,
            MAX_RUN_LOG_FILES,
        ),
        (
            "jobs.timeout_max_ms",
            jobs.timeout_max_ms,
            1,
            MAX_JOB_TIMEOUT_MS,
        ),
        (
            "jobs.timeout_default_ms",
            jobs.timeout_default_ms,
            1,
            jobs.timeout_max_ms,
        ),
        (
            "jobs.min_interval_s",
            jobs.min_interval_s,
            1,
            MAX_INTERVAL_S,
        ),
        (
            "jobs.check_period_s",
            jobs.check_period_s,
            1,
            MAX_CHECK_PERIOD_S,
        ),
        (
            "services.restart_max_ms",
            services.restart_max_ms,
            1,
            MAX_SERVICE_DELAY_MS,
        ),
        (
            "services.restart_initial_ms",
            services.restart_initial_ms,
            1,
            services.restart_max_ms,
        ),
        (
            "services.stable_after_ms",
            services.stable_after_ms,
            1,
            MAX_SERVICE_DELAY_MS,
        ),
    ];
    for (key, value, minimum, maximum) in ranges {
        if !(minimum..=maximum).contains(&value) {
            return Err(format!("{key} must be in {minimum}..={maximum}"));
        }
    }
    Ok(())
}

/// Describes why the knobs could not be read safely, by cause only.
fn unsafe_reason(error: &FsError) -> String {
    match error {
        FsError::FileTooLarge { .. } => format!("the file exceeds {MAX_FILE_BYTES} bytes"),
        FsError::Io { .. } => format!(
            "the file or its directory cannot be read: {}",
            error.io_kind().unwrap_or(ErrorKind::Other)
        ),
        _ => format!(
            "the directory must be owned by you and not writable by other accounts, even \
             without a {FILE_NAME}, and {FILE_NAME} must be a regular file owned by you with \
             mode {FILE_MODE:o}"
        ),
    }
}
