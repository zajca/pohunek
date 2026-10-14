//! The unit trampoline, `pohunek workflow __exec <id> <digest> job|service <name>`.
//!
//! Every job and service unit runs this hidden command of the installed
//! `<prefix>/bin/pohunek`, so the run policy is the same on systemd and
//! launchd and lives in pohunek, not in the unit:
//!
//! 1. It resolves the plugin store from the bootstrap roots the unit sets,
//!    reads the owner knobs, and requires the plugin to be installed and the
//!    `pohunek service` installation to describe this process. A unit whose
//!    plugin is gone is crash residue: it is refused with a message on stderr,
//!    since there is no run log to write to.
//! 2. A unit of another archive than the selected one, or of a disabled
//!    plugin, is refused and recorded at once, without starting anything. Otherwise it takes a
//!    non-blocking lock on `locks/<kind>-<name>.lock` in the
//!    plugin's state directory. A held lock means an earlier run of the same
//!    unit is still active; the run is recorded as `overlap_skipped` and the
//!    trampoline exits 0.
//! 3. A job keeps the wall-clock start of its last run in
//!    `schedule/job-<name>.json`. The unit's timer only checks every
//!    `jobs.check_period_s`; a check before `every_s` has passed exits 0
//!    without a record, so long intervals follow the wall clock across
//!    suspend and logins.
//! 4. A service keeps its count of short runs in
//!    `backoff/service-<name>.json` and waits the owner's restart delay for
//!    that count before it starts; a run of at least `services.stable_after_ms`
//!    starts the count over.
//! 5. The runner verifies the plugin again (enabled, the unit's digest
//!    selected, files intact, the unit declared), runs it without the
//!    session origin pair and with its output discarded, and records the
//!    outcome. A job's deadline is its `timeout_ms` or
//!    `jobs.timeout_default_ms`, below the unit's native start timeout.
//!
//! `SIGTERM`, `SIGINT` and `SIGHUP` are handled from the first step on: a
//! stop request during the restart delay ends the trampoline at once, and
//! one during a run stops the child's process group within the owner's kill
//! grace. Both service managers stop a unit by sending `SIGTERM` to the
//! trampoline alone (launchd always, systemd through the plugin units'
//! `KillMode=mixed`), so the trampoline sees the stop before its child
//! ends and stops the child's group itself. A stop by `SIGTERM` exits 0 once the group is stopped, so a stopped unit reads as stopped,
//! not failed; the run log still records the run as `interrupted`.
//! A refused service waits `services.restart_max_ms` before it exits, so the
//! service manager's immediate restart does not become a busy loop.

// Rust guideline compliant 2026-10-13

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::filesystem::{FileLock, FsError, LockKind, TrustedDir};
use pohunek_platform::supervisor::{PluginUnitKey, PluginUnitKind};
use pohunek_service_config::ServiceConfig;
use protocol::{PackageDigest, PackageId};
use serde::{Deserialize, Serialize};

use super::config::{self, Knobs, DEFAULT_RESTART_MAX_MS};
use super::runlog::Outcome;
use super::runner::{kind_name, run_supervised, Completed, Interrupts, Run, Supervised};
use super::store::Store;
use super::units::installation_at;
use super::Error;
use crate::paths::Paths;

/// Mode of the plugin state directory and of the subdirectories the
/// trampoline keeps in it.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of the lock and state files.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Subdirectory of the plugin state directory holding the run locks.
const LOCKS_DIR: &str = "locks";

/// Subdirectory holding each job's last run start.
const SCHEDULE_DIR: &str = "schedule";

/// Subdirectory holding each service's restart backoff.
const BACKOFF_DIR: &str = "backoff";

/// Largest state file read back; the files hold one small JSON object.
const MAX_STATE_FILE_BYTES: usize = 4096;

/// The stored count of a service's short runs when its backoff file cannot be
/// parsed. The restart delay of that count is the longest one, so a damaged
/// file never speeds up restarts.
pub(crate) const DAMAGED_BACKOFF: u32 = u32::MAX;

/// The stored count after a stable run, which starts the count over. It is
/// one, not zero, so the next start waits the initial restart delay; a
/// service's consecutive short runs are the stored count minus this.
pub(crate) const STABLE_BACKOFF_FAILURES: u32 = 1;

/// How much earlier than `every_s` a job check still counts as due.
///
/// launchd counts a job's interval from start to start, and the start time
/// is recorded a few milliseconds into the run, so a check exactly one
/// interval later can see slightly less than `every_s`; without the
/// tolerance that run would wait a whole further interval. One second is
/// the platforms' timer resolution for plugin units.
const DUE_TOLERANCE: Duration = Duration::from_secs(1);

/// How often a waiting trampoline checks for a stop request.
///
/// Short enough that a stop during the restart delay ends the trampoline
/// well within any kill grace; each check is one non-blocking poll.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// Status of a trampoline that refused or failed before its plugin ran.
const REFUSED_EXIT: u8 = 1;

/// Status of a job run stopped at its deadline, as for `workflow run`.
const TIMEOUT_EXIT: u8 = crate::commands::workflow::TIMEOUT_EXIT_CODE;

/// Offset POSIX shells add to a signal number in an exit status.
const SIGNAL_EXIT_BASE: i32 = 128;

/// One `__exec` request, as the unit's command line names it.
#[derive(Debug)]
pub(crate) struct Request {
    /// The plugin id.
    pub(crate) plugin: PackageId,
    /// The archive digest the unit was installed for.
    pub(crate) digest: PackageDigest,
    /// Whether the unit is a job or a service.
    pub(crate) kind: PluginUnitKind,
    /// The declared name.
    pub(crate) name: String,
}

impl Request {
    /// The requested unit name when it fits the unit name grammar, so a run
    /// record refused before the manifest verified can still name its unit;
    /// a name outside the grammar is never recorded.
    pub(crate) fn recordable_name(&self) -> Option<&str> {
        PluginUnitKey::new(self.plugin.as_str(), self.kind, &self.name)
            .is_ok()
            .then_some(self.name.as_str())
    }
}

/// The start of a job's last run.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    /// Wall-clock milliseconds since the Unix epoch.
    last_start_unix_ms: u64,
}

/// A service's count of short runs in a row.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backoff {
    /// Runs in a row that ended before `services.stable_after_ms`.
    failures: u32,
}

/// Runs the trampoline and returns its exit status.
///
/// # Panics
///
/// Panics when called outside a Tokio runtime, which registers the signal
/// handlers.
pub(crate) fn execute(request: &Request) -> ExitCode {
    let mut interrupts = match Interrupts::new() {
        Ok(interrupts) => interrupts,
        Err(error) => {
            eprintln!("workflow unit: the stop signals could not be handled: {error}");
            return ExitCode::from(REFUSED_EXIT);
        }
    };
    let unit = format!("{} {}", kind_name(request.kind), request.name);
    let id = &request.plugin;
    match prepare(request) {
        Ok((store, knobs)) => {
            let status = run(request, &store, &knobs, &mut interrupts);
            // A service that ran waits its backoff before its next run; a
            // refused one is paced here instead.
            let status = if status == Status::Refused && request.kind == PluginUnitKind::Service {
                pace(Some(&knobs), &mut interrupts).map_or(status, Status::Stopped)
            } else {
                status
            };
            status.into_exit()
        }
        Err(refusal) => {
            let (error, knobs) = *refusal;
            eprintln!("workflow {unit} of plugin {id} was refused: {error}");
            let status = if request.kind == PluginUnitKind::Service {
                pace(knobs.as_ref(), &mut interrupts).map_or(Status::Refused, Status::Stopped)
            } else {
                Status::Refused
            };
            status.into_exit()
        }
    }
}

/// How the trampoline ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Nothing ran: the job was not due or an earlier run is active.
    Skipped,
    /// The run was refused before it started.
    Refused,
    /// The trampoline was stopped before or during the run by this signal;
    /// any run's process group was stopped first.
    Stopped(i32),
    /// The plugin ran and ended with this status.
    Ran(u8),
}

impl Status {
    fn into_exit(self) -> ExitCode {
        match self {
            // A stop by `SIGTERM` is the service manager's own stop, which
            // the trampoline carried out.
            Self::Skipped | Self::Stopped(libc::SIGTERM) => ExitCode::SUCCESS,
            Self::Refused => ExitCode::from(REFUSED_EXIT),
            Self::Stopped(signal) => {
                ExitCode::from(u8::try_from(SIGNAL_EXIT_BASE + signal).unwrap_or(u8::MAX))
            }
            Self::Ran(code) => ExitCode::from(code),
        }
    }
}

/// A refusal before the run log could be opened, with the knobs when they
/// could be read.
type Early = Box<(Error, Option<Knobs>)>;

/// Checks the request and opens the store of an installed plugin.
fn prepare(request: &Request) -> Result<(Store, Knobs), Early> {
    let early = |error: Error| Box::new((error, None));
    PluginUnitKey::new(request.plugin.as_str(), request.kind, request.name.as_str()).map_err(
        |_invalid| {
            early(Error::UnitUnknown {
                id: request.plugin.to_string(),
                unit: format!("{} {:?}", kind_name(request.kind), request.name),
            })
        },
    )?;
    let dirs = Paths::workflow_dirs().map_err(|error| {
        early(Error::Store(format!(
            "the workflow plugin store cannot be located: {error}"
        )))
    })?;
    let store = Store::new(dirs);
    let knobs = config::load(store.knobs_path()).map_err(early)?;
    match store.installed(&request.plugin) {
        Ok(Some(_record)) => Ok((store, knobs)),
        Ok(None) => Err(Box::new((
            Error::PluginUnknown {
                id: request.plugin.to_string(),
            },
            Some(knobs),
        ))),
        Err(error) => Err(Box::new((error, Some(knobs)))),
    }
}

/// Runs an installed plugin's unit once, recording every outcome but a job
/// that is not due.
fn run(request: &Request, store: &Store, knobs: &Knobs, interrupts: &mut Interrupts) -> Status {
    let id = &request.plugin;
    let kind = kind_name(request.kind);
    let mut run = match Run::open(store, knobs, id, kind, 0) {
        Ok(run) => run,
        Err(error) => {
            eprintln!("workflow {kind} {} of plugin {id}: {error}", request.name);
            return Status::Refused;
        }
    };
    let Ok(Some(installed)) = store.installed(id) else {
        eprintln!(
            "workflow {kind} {} of plugin {id}: the plugin is not installed",
            request.name
        );
        return Status::Refused;
    };
    let refuse = |run: &mut Run, error: &Error| {
        eprintln!(
            "workflow {kind} {} of plugin {id} was refused: {error}",
            request.name
        );
        run.append(
            &installed,
            request.recordable_name(),
            Outcome::Refused,
            None,
            Some(error),
            [0; 2],
        );
        Status::Refused
    };
    let (cli, runtime_dir_ignored) = match installed_cli() {
        Ok(found) => found,
        Err(error) => return refuse(&mut run, &error),
    };
    let unit = Supervised {
        kind: request.kind,
        name: &request.name,
        digest: &request.digest,
        cli: &cli,
        runtime_dir_ignored,
    };
    // Refused from this snapshot, on a path that never starts anything; the
    // unit's lock, schedule and backoff belong to the installed archive and
    // stay as they are. The runner checks both again before it starts.
    if installed.digest() != &request.digest {
        let error = Error::UnitStale { id: id.to_string() };
        return refuse(&mut run, &error);
    }
    if !installed.enabled() {
        let error = Error::PluginDisabled { id: id.to_string() };
        return refuse(&mut run, &error);
    }
    let state = match TrustedDir::open_absolute(store.state_dir(id), PRIVATE_DIR_MODE) {
        Ok(state) => state,
        Err(error) => return refuse(&mut run, &state_error(&error)),
    };
    let _lock = match lock(&state, request) {
        Ok(lock) => lock,
        // The requested name is recorded when it fits the unit name grammar,
        // so status attributes the record to its unit.
        Err(FsError::LockContended { .. }) => {
            run.append(
                &installed,
                request.recordable_name(),
                Outcome::OverlapSkipped,
                None,
                None,
                [0; 2],
            );
            return Status::Skipped;
        }
        Err(error) => return refuse(&mut run, &state_error(&error)),
    };
    let backoff = match ready(request, store, knobs, &state, &mut run, interrupts) {
        Ok(Ready::Run(backoff)) => backoff,
        Ok(Ready::Skip(status)) => return status,
        Err(error) => return refuse(&mut run, &error),
    };
    let result = run_supervised(&mut run, store, knobs, id, &unit, interrupts);
    let status = match &result {
        Ok(Completed::Exited(status)) => Status::Ran(crate::service::exit_code(*status)),
        Ok(Completed::Document(_)) => unreachable!("a supervised run discards its stdout"),
        Err(Error::Interrupted { signal, .. }) => Status::Stopped(*signal),
        Err(Error::ActionTimeout { .. }) => Status::Ran(TIMEOUT_EXIT),
        Err(Error::SpawnFailed { .. } | Error::RunFailed { .. }) => Status::Ran(REFUSED_EXIT),
        Err(_refused) => Status::Refused,
    };
    if let Err(error) = &result {
        eprintln!("workflow {kind} {} of plugin {id}: {error}", request.name);
    }
    // Stability is the child's own lifetime: preparing the run and draining
    // its output do not count, and a run that never started a child is short.
    if let (Some(failures), Status::Ran(_)) = (backoff, status) {
        let ran = run.child_lifetime.unwrap_or(Duration::ZERO);
        save_backoff(request, knobs, &state, failures, ran);
    }
    status
}

/// Saves a service's count of short runs after a run of `ran`: one when the
/// run was stable, so the next delay starts over, else one more.
fn save_backoff(
    request: &Request,
    knobs: &Knobs,
    state: &TrustedDir,
    failures: u32,
    ran: Duration,
) {
    let failures = if ran >= Duration::from_millis(knobs.services.stable_after_ms) {
        STABLE_BACKOFF_FAILURES
    } else {
        failures.saturating_add(1)
    };
    if let Err(error) = write_state(state, BACKOFF_DIR, request, &Backoff { failures }) {
        eprintln!(
            "workflow {} {} of plugin {}: its restart backoff was not saved: {error}",
            kind_name(request.kind),
            request.name,
            request.plugin
        );
    }
}

/// The installed `<prefix>/bin/pohunek` of a verified service installation,
/// and whether an inherited `XDG_RUNTIME_DIR` was ignored to find it (see
/// [`trampoline_paths`]).
fn installed_cli() -> Result<(PathBuf, bool), Error> {
    let uid = nix::unistd::Uid::effective().as_raw();
    let (paths, ignored) = trampoline_paths(uid)?;
    installation_at(&paths, uid)?
        .map(|installation| (installation.cli, ignored))
        .ok_or_else(|| Error::ServiceNotInstalled {
            reason: "no service.toml exists".to_owned(),
        })
}

/// The base paths the trampoline runs with, resolved from its environment.
///
/// launchd merges its own environment into a unit's, and a unit pins every
/// root except `XDG_RUNTIME_DIR` when the installation uses the macOS default
/// runtime root. On macOS, a set `XDG_RUNTIME_DIR` is therefore ignored when
/// the recorded installation names that default root for this uid, so the
/// trampoline finds the installation it was installed into, and the returned
/// flag is set so the variable is not forwarded to the run's child either.
/// Any other case resolves as the environment says, and a mismatch is refused
/// by `installation_at`.
pub(crate) fn trampoline_paths(uid: u32) -> Result<(BasePaths, bool), Error> {
    let not_installed = |reason: String| Error::ServiceNotInstalled { reason };
    let platform = Platform::current().map_err(|error| not_installed(error.to_string()))?;
    let env = PathEnv::capture();
    let resolve = |env: &PathEnv| {
        BasePaths::resolve_for(platform, uid, env).map_err(|error| not_installed(error.to_string()))
    };
    if platform != Platform::MacOs || env.xdg_runtime_dir.is_none() {
        return Ok((resolve(&env)?, false));
    }
    // The config root does not depend on the runtime root, so the recorded
    // file is found through the default paths. An inherited variable that
    // is itself invalid is not consulted when the default root is recorded.
    let default = resolve(&PathEnv {
        xdg_runtime_dir: None,
        ..env.clone()
    })?;
    let recorded_default = ServiceConfig::load_upgrade_source(&pohunek_service_config::file_path(
        &default,
    ))
    .is_ok_and(|source| {
        source.config.uid() == uid && source.config.runtime_root() == default.runtime_dir
    });
    if recorded_default {
        Ok((default, true))
    } else {
        Ok((resolve(&env)?, false))
    }
}

/// Whether a locked unit runs now.
enum Ready {
    /// It runs; a service carries its count of short runs.
    Run(Option<u32>),
    /// It ends with this status without a run.
    Skip(Status),
}

/// Decides whether the locked unit runs now: a job when it is due, a
/// service after its restart delay, either unless a stop request arrived.
fn ready(
    request: &Request,
    store: &Store,
    knobs: &Knobs,
    state: &TrustedDir,
    run: &mut Run,
    interrupts: &mut Interrupts,
) -> Result<Ready, Error> {
    if let Some(signal) = interrupts.poll() {
        return Ok(Ready::Skip(Status::Stopped(signal)));
    }
    match request.kind {
        PluginUnitKind::Job => Ok(if due(store, state, request)? {
            Ready::Run(None)
        } else {
            Ready::Skip(Status::Skipped)
        }),
        PluginUnitKind::Service => {
            let failures = read_backoff(state, request)?;
            let delay = knobs.services.delay(failures);
            if let Some(signal) = wait(delay, interrupts) {
                return Ok(Ready::Skip(Status::Stopped(signal)));
            }
            run.backoff_ms = Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
            run.restart_clock()?;
            Ok(Ready::Run(Some(failures)))
        }
    }
}

/// Takes the unit's run lock without waiting.
fn lock(state: &TrustedDir, request: &Request) -> Result<FileLock, FsError> {
    state
        .open_or_create_child(LOCKS_DIR, PRIVATE_DIR_MODE)?
        .lock_file(
            format!("{}-{}.lock", kind_name(request.kind), request.name),
            PRIVATE_FILE_MODE,
            LockKind::Exclusive,
        )
}

/// Whether the job is due, recording its start when it is.
///
/// A manifest that cannot be read is due: the runner then refuses and
/// records the run. A start in the future, after the clock was set back,
/// is due too, so a clock change never stalls a job.
fn due(store: &Store, state: &TrustedDir, request: &Request) -> Result<bool, Error> {
    let every = match store.inspect(&request.plugin) {
        Ok((_record, _root, manifest)) => manifest
            .jobs
            .get(&request.name)
            .map(|job| Duration::from_secs(job.every_s.get())),
        Err(_unverified) => None,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_before_epoch| {
            Error::Store("the host clock is before the Unix epoch".to_owned())
        })?;
    let now_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    if let Some(every) = every {
        // A damaged file counts as no earlier run, so the job is not stalled.
        let last = match read_state::<Schedule>(state, SCHEDULE_DIR, request)? {
            Stored::Valid(schedule) => Some(schedule.last_start_unix_ms),
            Stored::Absent | Stored::Damaged => None,
        };
        if let Some(elapsed) = last.and_then(|last| now_ms.checked_sub(last)) {
            let elapsed = Duration::from_millis(elapsed);
            if elapsed + DUE_TOLERANCE < every {
                return Ok(false);
            }
        }
    }
    write_state(
        state,
        SCHEDULE_DIR,
        request,
        &Schedule {
            last_start_unix_ms: now_ms,
        },
    )?;
    Ok(true)
}

/// The service's count of short runs; a file that cannot be parsed counts
/// as [`DAMAGED_BACKOFF`].
fn read_backoff(state: &TrustedDir, request: &Request) -> Result<u32, Error> {
    Ok(match read_state::<Backoff>(state, BACKOFF_DIR, request)? {
        Stored::Absent => 0,
        Stored::Valid(backoff) => backoff.failures,
        Stored::Damaged => DAMAGED_BACKOFF,
    })
}

/// The service's stored count of short runs, read without creating anything:
/// a missing state directory or file counts as zero, as a fresh unit does.
///
/// # Errors
///
/// Returns [`Error::Store`] when the state directory is unusable.
pub(crate) fn backoff_of(state_dir: &Path, request: &Request) -> Result<u32, Error> {
    let not_found = |error: &FsError| error.io_kind() == Some(io::ErrorKind::NotFound);
    let state = match TrustedDir::open_absolute(state_dir, PRIVATE_DIR_MODE) {
        Ok(state) => state,
        Err(error) if not_found(&error) => return Ok(0),
        Err(error) => return Err(state_error(&error)),
    };
    let dir = match state.open_child(BACKOFF_DIR, PRIVATE_DIR_MODE) {
        Ok(dir) => dir,
        Err(error) if not_found(&error) => return Ok(0),
        Err(error) => return Err(state_error(&error)),
    };
    let bytes = match dir.read_file(state_file(request), PRIVATE_FILE_MODE, MAX_STATE_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if not_found(&error) => return Ok(0),
        Err(error) => return Err(state_error(&error)),
    };
    Ok(serde_json::from_slice::<Backoff>(&bytes)
        .map_or(DAMAGED_BACKOFF, |backoff| backoff.failures))
}

/// What a unit state file holds.
enum Stored<T> {
    /// There is no file.
    Absent,
    /// The file parses.
    Valid(T),
    /// The file does not parse.
    Damaged,
}

/// The state file name of the unit.
fn state_file(request: &Request) -> String {
    format!("{}-{}.json", kind_name(request.kind), request.name)
}

/// Reads the unit's state file in `dir`.
fn read_state<T: for<'de> Deserialize<'de>>(
    state: &TrustedDir,
    dir: &str,
    request: &Request,
) -> Result<Stored<T>, Error> {
    let dir = state
        .open_or_create_child(dir, PRIVATE_DIR_MODE)
        .map_err(|error| state_error(&error))?;
    let bytes = match dir.read_file(state_file(request), PRIVATE_FILE_MODE, MAX_STATE_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            return Ok(Stored::Absent);
        }
        Err(error) => return Err(state_error(&error)),
    };
    Ok(serde_json::from_slice(&bytes).map_or(Stored::Damaged, Stored::Valid))
}

/// Atomically replaces the unit's state file in `dir`.
fn write_state(
    state: &TrustedDir,
    dir: &str,
    request: &Request,
    value: &impl Serialize,
) -> Result<(), Error> {
    let dir = state
        .open_or_create_child(dir, PRIVATE_DIR_MODE)
        .map_err(|error| state_error(&error))?;
    let bytes = serde_json::to_vec(value).expect("a unit state holds only integers");
    let name = state_file(request);
    let temporary = format!(".{name}.{}.tmp", std::process::id());
    dir.replace_file(&name, &temporary, &bytes, PRIVATE_FILE_MODE)
        .map_err(|error| Error::Store(format!("{name} was not saved: {error}")))
}

/// Describes a state directory failure by its cause only.
fn state_error(error: &FsError) -> Error {
    Error::Store(format!(
        "the plugin state directory is unusable: {}",
        match error {
            FsError::UnsafeMode { .. }
            | FsError::UnsafeOwner { .. }
            | FsError::UnsafeAcl { .. } => {
                "an entry is not owned by you with owner-only permissions".to_owned()
            }
            FsError::UnsafeType { .. } => {
                "an entry is a symbolic link or special file".to_owned()
            }
            other => other
                .io_kind()
                .map_or_else(|| "filesystem failure".to_owned(), |kind| kind.to_string()),
        }
    ))
}

/// Waits `delay` unless a stop signal arrives first; returns that signal.
fn wait(delay: Duration, interrupts: &mut Interrupts) -> Option<i32> {
    let until = Instant::now() + delay;
    loop {
        if let Some(signal) = interrupts.poll() {
            return Some(signal);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        thread::sleep(WAIT_POLL.min(left));
    }
}

/// Paces a refused service by the owner's longest restart delay, or its
/// documented default when the knobs could not be read. Returns the stop
/// signal that ended the pacing early, if one arrived.
fn pace(knobs: Option<&Knobs>, interrupts: &mut Interrupts) -> Option<i32> {
    let delay = knobs.map_or(DEFAULT_RESTART_MAX_MS, |knobs| {
        knobs.services.restart_max_ms
    });
    wait(Duration::from_millis(delay), interrupts)
}
