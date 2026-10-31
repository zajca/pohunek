//! `pohunekd` — the pohunek host daemon binary.
//!
//! Milestone 2: resolve XDG paths, initialize JSON logging, acquire the
//! single-instance lock, bind the control socket (with stale-socket recovery and
//! owner-private permissions), and serve `daemon.health` until SIGINT/SIGTERM.
//!
//! The daemon binds one TCP control listener for each configured overlay and
//! serves them alongside the local Unix socket under one shutdown. A provider
//! being temporarily unavailable is not an error: the daemon stays reachable
//! through healthy transports while retrying that listener.
//!
//! Worker supervision is selected explicitly:
//!
//! - `pohunekd --service-config <absolute service.toml>` supervises workers as
//!   native jobs (transient systemd units on Linux, launchd jobs on macOS)
//!   with every value from the installed configuration;
//! - `POHUNEK_WORKER_LAUNCHER=subprocess` without `--service-config` runs
//!   workers as direct children under the documented dev/test contract.
//!
//! Anything else fails at startup.
//!
//! `pohunekd upgrade-preflight` is the one other mode. It prints, as one JSON
//! document, whether this build would adopt the live sessions of the installed
//! state, reading the store, the worker journals and the process table only. It
//! runs before any lock, directory, log or socket is touched, so it is safe
//! while the previous release's daemon runs; `pohunek service check|upgrade`
//! invoke it.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use overlay::{ConfiguredTransport, OverlayId, OverlayRegistry};
use pohunek_daemon::api::{
    ConnectionTasks, ControlServer, DaemonState, HealthInfo, OpenedListener, OverlaySource,
    RemoteServer,
};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::discovery::DiscoveryCache;
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::lock::InstanceLock;
use pohunek_daemon::notifications::{
    spawn_notification_retention, spawn_projector, AttentionCoordinator, NotificationService,
    NOTIFICATIONS_SUBDIR,
};
use pohunek_daemon::runtime::lifecycle::{
    LogNaming, DEV_OPEN_FILES, DEV_SWEEP_GRACE, DEV_WORKER_CONNECT, DEV_WORKER_EXIT_TIMEOUT,
    DEV_WORKER_INITIALIZE,
};
use pohunek_daemon::runtime::{
    EnvironmentSource, LaunchEnvironment, SubprocessWorkerLauncher, SupervisionConfig,
    WorkerLauncher,
};
use pohunek_daemon::session::upgrade_preflight::{
    self, PreflightInputs, PreflightOptions, ReleaseInputs,
};
use pohunek_daemon::session::{
    detect_official_runtimes, ensure_official_runtimes, spawn_idle_stop, spawn_session_retention,
    EnsureInputs, SessionRegistry, SessionRegistryConfig, IDLE_STOP_POLICY_NAME,
    RETENTION_POLICY_NAME,
};
use pohunek_daemon::shutdown::{drop_runtime, DaemonShutdown, ShutdownPlan, Stage};
use pohunek_daemon::{logging, DaemonError, Paths, DAEMON_VERSION};
use pohunek_platform::supervisor::{self, Namespace};
use pohunek_service_config::preflight::{
    MAX_PROBE_BUDGET_SECS, MIN_PROBE_BUDGET_SECS, PROBE_BUDGET, PROBE_BUDGET_FLAG,
    RELEASE_PLAN_FLAG,
};
use pohunek_service_config::runtimes::{
    AcceptedGap, CatalogStatus, EnsureReport, Refusal, ACCEPT_GAP_FLAG, CODE_DAEMON_RUNNING,
    DETECT_FLAG, DRY_RUN_FLAG, ENSURE_REPORT_VERSION, ENSURE_RUNTIMES_ARG, RELEASE_RUNTIME_DIR,
};
use pohunek_service_config::{InputTiming, ServiceConfig, WaitTiming, RUNTIME_DROP_BOUND};

/// File name of the unified logical-session metadata store under the data dir.
const STORE_NAME: &str = pohunek_paths::METADATA_STORE_NAME;

/// Subdirectory under the data dir holding per-session git worktrees.
const WORKTREES_SUBDIR: &str = "worktrees";
/// Directory under the cache dir that holds, per store, each session's private
/// `TMPDIR`.
const SESSION_TMP_SUBDIR: &str = "session-tmp";

/// Subdirectory under the data dir holding the append-only event log.
const EVENTS_SUBDIR: &str = "events";
/// Per-session worker socket root under the owner-private runtime directory.
const WORKERS_SUBDIR: &str = pohunek_paths::WORKERS_SUBDIR;

/// Env var enabling opt-in observation of agents outside pohunek-owned PTYs.
const OBSERVE_EXTERNAL_AGENTS_ENV: &str = "POHUNEK_OBSERVE_EXTERNAL_AGENTS";
/// Selects the dev/test supervision mode; its only accepted value is
/// [`SUBPROCESS_LAUNCHER`]. Native supervision is selected by
/// [`SERVICE_CONFIG_FLAG`] instead.
const WORKER_LAUNCHER_ENV: &str = "POHUNEK_WORKER_LAUNCHER";
/// Value of [`WORKER_LAUNCHER_ENV`] that runs `pohunek-sessiond` as direct
/// children, for headless development and tests without a service manager.
const SUBPROCESS_LAUNCHER: &str = "subprocess";
/// Command-line flag naming the installed `service.toml`.
const SERVICE_CONFIG_FLAG: &str = "--service-config";
/// Overrides the durable worker binary path used by the subprocess launcher.
/// When unset, the daemon uses the `pohunek-sessiond` co-located next to itself.
const WORKER_BIN_ENV: &str = "POHUNEK_WORKER_BIN";
/// Bootstrap variables forwarded to worker jobs when the daemon has them.
///
/// A worker resolves its paths from exactly these, so forwarding the daemon's
/// own values makes both resolve the same tree; unset variables fall back to
/// the same `HOME`-relative or platform defaults on both sides.
const BOOTSTRAP_VARIABLES: [&str; 6] = [
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "HOME",
];
/// Durable worker binary name, expected next to the daemon executable.
const WORKER_BINARY_NAME: &str = "pohunek-sessiond";

/// Initial delay between attempts to enable an optional remote listener.
///
/// An overlay can report incomplete state while its local service and interface
/// are starting. Five seconds avoids repeatedly querying the provider while
/// making a newly ready host discoverable without an operator restart.
const REMOTE_BIND_INITIAL_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Longest delay between remote-listener retry attempts.
///
/// A local-only daemon is valid when an overlay is not ready, so retries back
/// off to avoid continuously querying its state and filling logs. Five minutes
/// still recovers reasonably quickly after a delayed login or interface setup.
const REMOTE_BIND_MAX_RETRY_INTERVAL: Duration = Duration::from_mins(5);

/// Interval for revalidating an active overlay listener address.
///
/// Provider-owned interface addresses can change while the daemon remains up.
/// Thirty seconds bounds stale listener exposure without continuously polling
/// the provider CLI.
const REMOTE_LISTENER_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Sole argument that prints the binary name and version and exits.
///
/// `pohunek service install` runs it to confirm a staged executable before
/// referencing it, so it is answered before any environment, path, logging,
/// lock, or socket work.
const VERSION_FLAG: &str = "--version";

/// Sole argument that prints the adoption preflight of the installed state.
///
/// Like [`VERSION_FLAG`] it is answered before any environment-dependent
/// startup work: it takes no instance lock, creates no directory and
/// initializes no logging, because the previous release's daemon owns all of
/// them while the preflight runs.
const UPGRADE_PREFLIGHT_ARG: &str = "upgrade-preflight";

/// Name of the host agent profiles directory under the config directory.
const PROFILES_SUBDIR: &str = "agents";

fn main() -> ExitCode {
    // Before the runtime or any other thread exists, so every thread shares the bounded arenas.
    // SAFETY: first statement of `main`; no other thread exists.
    let _ = unsafe { pohunek_daemon::malloc::bound_arenas() };
    // Declared before the runtime so it drops after it, on a return and while
    // a panic unwinds out of `block_on`: the log writer flushes last.
    let _flush_logs = FlushLogs;
    // The runtime is built here, not by `#[tokio::main]`, because dropping it
    // waits for blocking threads without a bound; `drop_runtime` bounds it.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("pohunekd: fatal: cannot build the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(daemon_main());
    drop_runtime(runtime, RUNTIME_DROP_BOUND);
    code
}

/// Keeps the log writer's worker alive until `main` has dropped the runtime.
static LOG_GUARD: Mutex<Option<logging::LogGuard>> = Mutex::new(None);

/// Drops the log writer's guard, which flushes the queued log lines.
struct FlushLogs;

impl Drop for FlushLogs {
    fn drop(&mut self) {
        drop(
            LOG_GUARD
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }
}

/// Logs once what [`pohunek_daemon::malloc::bound_arenas`] did.
fn log_arena_bound() {
    use pohunek_daemon::malloc::{recorded_bound, ArenaBound};
    match recorded_bound() {
        Some(ArenaBound::Applied { limit, threads }) => {
            info!(
                malloc_arena_max = limit,
                threads_at_bound = threads,
                "malloc arenas bounded"
            );
        }
        Some(ArenaBound::OperatorOverride) => {
            info!("malloc arena limit named by the operator environment; mallopt not called");
        }
        Some(ArenaBound::Rejected { limit }) => {
            warn!(
                malloc_arena_max = limit,
                "mallopt refused the malloc arena bound"
            );
        }
        Some(ArenaBound::Unsupported) | None => {}
    }
}

async fn daemon_main() -> ExitCode {
    // The probe deadline of `upgrade-preflight` counts from here.
    let entry = std::time::Instant::now();
    if std::env::args_os()
        .skip(1)
        .eq([OsString::from(VERSION_FLAG)])
    {
        return print_version();
    }
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    if arguments.first().map(OsString::as_os_str) == Some(OsStr::new(UPGRADE_PREFLIGHT_ARG)) {
        return match parse_upgrade_preflight_arguments(&arguments[1..]) {
            Ok((release_plan, probe_budget)) => {
                print_upgrade_preflight(release_plan, probe_budget, entry)
            }
            Err(error) => {
                eprintln!("pohunekd: {UPGRADE_PREFLIGHT_ARG}: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if std::env::args_os()
        .skip(1)
        .eq([OsString::from(ENSURE_RUNTIMES_ARG)])
    {
        return print_ensure_runtimes(false, Vec::new());
    }
    if arguments.first() == Some(&OsString::from(ENSURE_RUNTIMES_ARG))
        && arguments[1..].contains(&OsString::from(ACCEPT_GAP_FLAG))
    {
        return match parse_accepted_gaps(&arguments[1..]) {
            Ok((dry_run, accepted)) => print_ensure_runtimes(dry_run, accepted),
            Err(error) => {
                eprintln!("pohunekd: {ENSURE_RUNTIMES_ARG}: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if std::env::args_os().skip(1).eq([
        OsString::from(ENSURE_RUNTIMES_ARG),
        OsString::from(DRY_RUN_FLAG),
    ]) {
        return print_ensure_runtimes(true, Vec::new());
    }
    if std::env::args_os().skip(1).eq([
        OsString::from(ENSURE_RUNTIMES_ARG),
        OsString::from(DRY_RUN_FLAG),
        OsString::from(DETECT_FLAG),
    ]) || std::env::args_os().skip(1).eq([
        OsString::from(ENSURE_RUNTIMES_ARG),
        OsString::from(DETECT_FLAG),
        OsString::from(DRY_RUN_FLAG),
    ]) {
        return print_ensure_runtimes_detect();
    }
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    if arguments.first() == Some(&OsString::from(ENSURE_RUNTIMES_ARG))
        && arguments[1..].contains(&OsString::from(DETECT_FLAG))
    {
        eprintln!(
            "pohunekd: {DETECT_FLAG} needs {DRY_RUN_FLAG} and plans only; setup applies the plan \
             through the running daemon"
        );
        return ExitCode::FAILURE;
    }
    // The startup future is large (reconciliation, listeners, event logs); box
    // it so it lives on the heap instead of inflating the `main` task frame.
    match Box::pin(run()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Logging may not be initialized yet (e.g. missing env), so also
            // print to stderr to guarantee the operator sees the failure.
            eprintln!("pohunekd: fatal: {err}");
            error!(error = %err, "daemon exited with error");
            ExitCode::FAILURE
        }
    }
}

/// Parses the arguments after `upgrade-preflight`: an optional `--release-plan`
/// and an optional `--probe-budget-secs <SECONDS>`, in that order. Without the
/// budget the default of [`PROBE_BUDGET`] applies.
fn parse_upgrade_preflight_arguments(arguments: &[OsString]) -> Result<(bool, Duration), String> {
    let mut rest = arguments;
    let release_plan = match rest.first() {
        Some(first) if first.as_os_str() == OsStr::new(RELEASE_PLAN_FLAG) => {
            rest = &rest[1..];
            true
        }
        _ => false,
    };
    let probe_budget = match rest {
        [] => PROBE_BUDGET,
        [flag, value] if flag.as_os_str() == OsStr::new(PROBE_BUDGET_FLAG) => {
            let text = value
                .to_str()
                .ok_or_else(|| "the probe budget is not valid text".to_owned())?;
            let seconds: u64 = text.parse().map_err(|error| {
                format!("the probe budget `{text}` is not a whole number of seconds: {error}")
            })?;
            if !(MIN_PROBE_BUDGET_SECS..=MAX_PROBE_BUDGET_SECS).contains(&seconds) {
                return Err(format!(
                    "the probe budget must be from {MIN_PROBE_BUDGET_SECS} to {MAX_PROBE_BUDGET_SECS} seconds, not {seconds}"
                ));
            }
            Duration::from_secs(seconds)
        }
        _ => return Err("unexpected arguments".to_owned()),
    };
    Ok((release_plan, probe_budget))
}

/// Prints the preflight report of the installed state as JSON.
///
/// With `release_plan` the judgement also serves the runtimes the release step
/// would leave behind; without it the judgement serves the installed packages
/// only, as a rollback's preflight must. `probe_budget` bounds the version
/// probes and reference checks, counted from `entry`.
fn print_upgrade_preflight(
    release_plan: bool,
    probe_budget: Duration,
    entry: std::time::Instant,
) -> ExitCode {
    let report = Paths::resolve()
        .map_err(|error| error.to_string())
        .and_then(|paths| {
            let inputs = PreflightInputs {
                store_path: paths.data_dir.join(STORE_NAME),
                worker_state_root: paths.state_dir.join(WORKERS_SUBDIR),
                plugins_dir: paths.plugins_dir(),
            };
            let release_dir = release_runtime_dir()?;
            let options = PreflightOptions {
                release: (release_plan && release_dir.is_dir()).then(move || ReleaseInputs {
                    release_dir,
                    catalog_trust: CatalogTrust::load_beside_executable(),
                }),
                profiles_dir: Some(paths.config_dir.join(PROFILES_SUBDIR)),
                host_state_dir: Some(paths.state_dir.clone()),
                probe_budget,
                entry,
                launch: launch_environment(&paths, false),
            };
            let inspector = pohunek_daemon::procwatch::HostInspector::new();
            let report = upgrade_preflight::run_with(&inputs, &options, &inspector);
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())
        });
    match report {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("pohunekd: upgrade-preflight failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The launch environment the installed daemon's `service.toml` configures,
/// read as daemon startup reads it, or the reason it cannot be read.
///
/// With `detect`, a host without an installed service file is planned with the
/// environment `service install` gives its daemon: the default allowlist over
/// the process environment. Setup previews before it installs the service, so
/// that host is the normal case for the detection plan. Any other failure stays
/// a refusal.
fn launch_environment(paths: &Paths, detect: bool) -> Result<LaunchEnvironment, String> {
    let path = paths.config_dir.join(pohunek_service_config::FILE_NAME);
    if detect {
        return LaunchEnvironment::from_service_file_if_present(&path, EnvironmentSource::Process)
            .map(|environment| environment.unwrap_or_else(LaunchEnvironment::subprocess_default));
    }
    LaunchEnvironment::from_service_file(&path, EnvironmentSource::Process)
}

/// The release `runtime/` directory beside the running daemon executable.
fn release_runtime_dir() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(executable
        .parent()
        .ok_or_else(|| "the daemon executable has no directory".to_owned())?
        .join(RELEASE_RUNTIME_DIR))
}

/// Prints the offline runtime step's report as JSON, or its failure on stderr.
fn print_ensure_runtimes(dry_run: bool, accepted_gaps: Vec<AcceptedGap>) -> ExitCode {
    print_ensure_report(ensure_runtimes(dry_run, false, accepted_gaps))
}

/// Parses the arguments after `ensure-official-runtimes` of a run that
/// accepts gaps: an optional [`DRY_RUN_FLAG`], then [`ACCEPT_GAP_FLAG`] with a
/// `<runtime>:<code>` value, repeated. Returns whether the run is a dry run
/// and the accepted gaps. Anything else is an error, so a mistyped flag never
/// starts the daemon.
fn parse_accepted_gaps(arguments: &[OsString]) -> Result<(bool, Vec<AcceptedGap>), String> {
    let mut accepted = Vec::new();
    let dry_run = arguments.first().map(OsString::as_os_str) == Some(OsStr::new(DRY_RUN_FLAG));
    let mut rest = arguments[usize::from(dry_run)..].iter();
    while let Some(flag) = rest.next() {
        if flag.as_os_str() != OsStr::new(ACCEPT_GAP_FLAG) {
            return Err(format!(
                "{ACCEPT_GAP_FLAG} <runtime>:<code> is the only argument a run takes with it, \
                 after an optional {DRY_RUN_FLAG}, not {}",
                flag.to_string_lossy()
            ));
        }
        let value = rest
            .next()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("{ACCEPT_GAP_FLAG} needs a <runtime>:<code> value"))?;
        accepted.push(
            AcceptedGap::from_arg(value).ok_or_else(|| {
                format!("{ACCEPT_GAP_FLAG} takes <runtime>:<code>, not `{value}`")
            })?,
        );
    }
    Ok((dry_run, accepted))
}

/// Prints the detection plan of the runtime step as JSON, or its failure on
/// stderr. The plan changes nothing.
fn print_ensure_runtimes_detect() -> ExitCode {
    print_ensure_report(ensure_runtimes(true, true, Vec::new()))
}

fn print_ensure_report(report: Result<EnsureReport, String>) -> ExitCode {
    let report = report.and_then(|report| {
        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())
    });
    match report {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("pohunekd: {ENSURE_RUNTIMES_ARG} failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the offline runtime step over the installed state.
///
/// A dry run takes no lock and creates nothing. An apply holds the data
/// authority and the instance locks until it ends, so a daemon cannot start
/// underneath it; a held lock is a refusal, because the running daemon owns
/// the state and nothing may change.
fn ensure_runtimes(
    dry_run: bool,
    detect: bool,
    accepted_gaps: Vec<AcceptedGap>,
) -> Result<EnsureReport, String> {
    let paths = Paths::resolve().map_err(|error| error.to_string())?;
    let release_dir = release_runtime_dir()?;
    let inputs = EnsureInputs {
        store_path: paths.data_dir.join(STORE_NAME),
        plugins_dir: paths.plugins_dir(),
        profiles_dir: paths.config_dir.join(PROFILES_SUBDIR),
        release_dir,
        catalog_trust: CatalogTrust::load_beside_executable(),
        dry_run,
        daemon_version: DAEMON_VERSION.to_owned(),
        environment: launch_environment(&paths, detect),
        accepted_gaps,
    };
    if detect {
        return detect_official_runtimes(&inputs)
            .map(|plan| plan.report)
            .map_err(|error| error.to_string());
    }
    if dry_run {
        return ensure_official_runtimes(&inputs).map_err(|error| error.to_string());
    }
    ensure_private_dir(&paths.data_dir).map_err(|error| error.to_string())?;
    ensure_private_dir(&paths.runtime_dir).map_err(|error| error.to_string())?;
    let mut locks = vec![paths.data_authority_lock_path()];
    if paths.runtime_dir != paths.data_dir {
        locks.push(paths.lock.clone());
    }
    let mut held = Vec::with_capacity(locks.len());
    for path in &locks {
        match InstanceLock::acquire(path) {
            Ok(lock) => held.push(lock),
            Err(DaemonError::AlreadyRunning { .. }) => {
                return Ok(EnsureReport {
                    report_version: ENSURE_REPORT_VERSION,
                    daemon_version: DAEMON_VERSION.to_owned(),
                    dry_run,
                    catalog: CatalogStatus::Unavailable,
                    packages: Vec::new(),
                    profiles: Vec::new(),
                    integrations: Vec::new(),
                    refusals: vec![Refusal {
                        subject: "daemon".to_owned(),
                        code: CODE_DAEMON_RUNNING.to_owned(),
                        detail: "a daemon holds the instance lock; stop it before the runtime step"
                            .to_owned(),
                    }],
                    gaps: Vec::new(),
                });
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let report = ensure_official_runtimes(&inputs).map_err(|error| error.to_string());
    drop(held);
    report
}

#[expect(
    clippy::too_many_lines,
    reason = "Daemon startup deliberately keeps ordered readiness gates in one auditable sequence."
)]
async fn run() -> Result<(), DaemonError> {
    // 1. Select worker supervision and resolve all paths up front; fail fast on
    //    a missing mode or required env.
    let mode = select_supervision(
        parse_service_config(std::env::args_os().skip(1))?,
        std::env::var_os(WORKER_LAUNCHER_ENV),
    )?;
    let paths = Paths::resolve()?;

    // The overlay configuration decides which network the remote listener
    // binds, so an unusable one stops the daemon before any lock, store
    // migration or worker adoption.
    let overlay_config = pohunek_client::OverlayConfig::load_from(&paths.config_dir)
        .map_err(DaemonError::OverlayConfiguration)?;
    let registry = overlay_config
        .registry()
        .map_err(DaemonError::OverlayConfiguration)?;
    let overlay_source = OverlaySource::of(&overlay_config);

    // 2. Acquire the durable data authority before opening any state service.
    // State and data roots are configured independently, so each needs its own
    // lifetime lock. Preparing the data root is safe before the lock because it
    // creates only the owner-private directory that contains the lock itself.
    ensure_private_dir(&paths.data_dir)?;
    let _data_authority = InstanceLock::acquire(&paths.data_authority_lock_path())?;

    // 3. Acquire the durable host-state authority. Two daemon processes can
    // intentionally use different explicit runtime roots, so the runtime lock
    // cannot arbitrate ownership of either durable tree.
    let governance = Arc::new(HostGovernanceService::open(paths.state_dir.clone()).await?);

    // 4. Prepare the owner-private runtime root only after durable authority is
    // held. The control server revalidates it on bind, while the runtime lock
    // rejects duplicate use of this exact runtime namespace.
    ensure_private_dir(&paths.runtime_dir)?;
    ensure_private_dir(&paths.runtime_dir.join(WORKERS_SUBDIR))?;
    let _runtime_authority = if paths.runtime_dir == paths.data_dir {
        // The retained data authority already holds the shared directory inode.
        // Reopening and flocking that inode would contend with this process's
        // own lock even though the marker filenames differ.
        None
    } else {
        Some(InstanceLock::acquire(&paths.lock)?)
    };

    // 5. Initialize structured logging and durable worker state below B1's
    // owner-private application state directory.
    *LOG_GUARD.lock().unwrap_or_else(PoisonError::into_inner) =
        Some(logging::init(&paths.log_dir)?);
    ensure_private_dir(&paths.state_dir.join(WORKERS_SUBDIR))?;
    info!(
        daemon_version = DAEMON_VERSION,
        runtime_dir = %paths.runtime_dir.display(),
        log_dir = %paths.log_dir.display(),
        "pohunekd starting"
    );
    info!(lock = %paths.lock.display(), "acquired single-instance lock");
    log_arena_bound();

    // 6. Build the session registry with the hook-handshake socket path, the
    //    unified metadata store (logical sessions and related bindings), the
    //    worktrees root, and the event-log directory, so spawned agents can
    //    report native identity, logical sessions survive a restart, a
    //    repo+branch session binds a dedicated worktree, and the lifecycle is
    //    recorded to the append-only event log.
    let store_path = paths.data_dir.join(STORE_NAME);
    // The schema migration runs before the registry exists so reconciliation and
    // adoption only ever see a store at the current schema.
    pohunek_daemon::store::migrate_at_startup(&store_path)?;
    let session_tmp_root = pohunek_daemon::session::store_session_tmp_root(
        &paths.cache_dir.join(SESSION_TMP_SUBDIR),
        &store_path,
    )?;
    let config = SessionRegistryConfig {
        socket_path: Some(paths.socket.clone()),
        store_path: Some(store_path),
        worktree_root: Some(paths.data_dir.join(WORKTREES_SUBDIR)),
        session_tmp_root: Some(session_tmp_root),
        event_log_dir: Some(paths.data_dir.join(EVENTS_SUBDIR)),
        retention_policy_path: Some(paths.data_dir.join(RETENTION_POLICY_NAME)),
        idle_stop_policy_path: Some(paths.data_dir.join(IDLE_STOP_POLICY_NAME)),
        log_dir: Some(paths.log_dir.clone()),
        // Slice 0 owns this line: B3 (host-global hooks) and C1 (agent profiles)
        // read it through SessionRegistry::config_dir() / derive off it — they must
        // NOT re-add config_dir here.
        config_dir: Some(paths.config_dir.clone()),
        // Part C: host agent profiles live under <config_dir>/agents.
        agents_dir: Some(paths.config_dir.join("agents")),
        host_state_dir: Some(paths.state_dir.clone()),
        plugins_dir: Some(paths.plugins_dir()),
        // The anchor file beside the executable authorizes catalog installs;
        // an absent or untrusted one fails them closed.
        catalog_trust: CatalogTrust::load_beside_executable(),
        observe_external_agents: env_bool(OBSERVE_EXTERNAL_AGENTS_ENV)?,
        worker_runtime_root: Some(paths.runtime_dir.join(WORKERS_SUBDIR)),
        worker_state_root: Some(paths.state_dir.join(WORKERS_SUBDIR)),
        ..SessionRegistryConfig::default()
    };
    let (sessions, shutdown_plan) = build_session_registry(config, &paths, mode).await?;
    let tasks = DaemonShutdown::new(shutdown_plan);
    // Load the durable retention policy before anything can sweep: a corrupt or
    // unsafely-permissioned document must fail startup rather than silently
    // revert to the shipped default.
    let retention_policy = sessions
        .load_retention_policy()
        .await
        .map_err(DaemonError::RetentionPolicy)?;
    info!(
        session.retention_enabled = retention_policy.enabled,
        session.retention_sweep_interval_secs = retention_policy.sweep_interval_secs,
        session.retention_terminal_ttl_secs = retention_policy.terminal_ttl_secs,
        session.retention_lost_ttl_secs = retention_policy.lost_ttl_secs,
        "loaded session retention policy"
    );
    // The idle-stop default fails startup the same way: a corrupt record must not
    // silently turn into "no default".
    let idle_stop_policy = sessions
        .load_idle_stop_policy()
        .await
        .map_err(DaemonError::IdleStopPolicy)?;
    info!(
        session.idle_stop_default_secs = idle_stop_policy.default_after_secs,
        session.idle_stop_min_secs = idle_stop_policy.min_after_secs,
        "loaded session idle-stop default"
    );
    let notifications =
        NotificationService::open(&paths.data_dir).map_err(|source| DaemonError::Directory {
            path: paths.data_dir.join(NOTIFICATIONS_SUBDIR),
            source: std::io::Error::other(source),
        })?;

    // 7. Start the append-only event log before anything emits, so worker
    //    reconciliation events are captured too. The log drains
    //    both session and notification control-plane events into one file.
    spawn_event_logs(
        &tasks,
        &paths.data_dir.join(EVENTS_SUBDIR),
        &sessions,
        &notifications,
    )?;
    spawn_notification_retention(&tasks, notifications.clone());
    spawn_session_retention(&tasks, sessions.clone());
    spawn_idle_stop(&tasks, sessions.clone(), notifications.clone());
    // Spawn the debounce coordinator: it owns the lifecycle of session-scoped
    // agent_blocked/approval_required and turn_completed notifications, holding
    // them for the policy debounce window so transient signals never surface,
    // and records the surviving signals as turn events for session.wait_turn.
    // Both producers (the notification.create handler and the projector) route
    // debounced notifications through its clonable command handle.
    let (attention_coordinator, _attention_task) =
        AttentionCoordinator::spawn(&tasks, notifications.clone(), &sessions);
    spawn_projector(
        &tasks,
        &sessions,
        notifications.clone(),
        attention_coordinator.clone(),
    );
    sessions.spawn_agent_state_hooks_in(&tasks);

    // 7. Adopt exact surviving worker runtimes before exposing the public API.
    //    Reconciliation never invokes provider-native resume.
    Box::pin(sessions.reconcile_workers())
        .await
        .map_err(DaemonError::Reconcile)?;

    // 8. Bind the control socket (stale-socket recovery + 0600).
    let health = HealthInfo::new(DAEMON_VERSION);
    let discovery = DiscoveryCache::new(registry.clone());
    let state = DaemonState::new_with_discovery(
        health,
        sessions.clone(),
        Arc::clone(&governance),
        discovery.clone(),
        overlay_source,
    )
    .with_notifications(notifications.clone())
    .with_attention_coordinator(attention_coordinator.clone());
    let package_commands = state.package_commands.clone();
    let shutdown_package_commands = package_commands.clone();
    tasks.add_step("package_commands", Stage::Driver, move || async move {
        shutdown_package_commands.shutdown().await;
    });
    let server = ControlServer::bind_with_state(&paths.socket, state).await?;
    info!(socket = %server.socket_path().display(), "ready; serving control protocol");

    // 9. Keep attempting to bind every configured overlay listener alongside
    //    the Unix socket. Providers may become ready after this daemon starts.
    let remote_state = DaemonState::new_with_discovery(
        HealthInfo::new(DAEMON_VERSION),
        sessions.clone(),
        governance,
        discovery,
        overlay_source,
    )
    .with_notifications(notifications.clone())
    .with_attention_coordinator(attention_coordinator.clone())
    .with_package_commands(package_commands.clone());
    // Reconciliation and every required local listener are ready. A manual
    // foreground launch has no NOTIFY_SOCKET and this is a no-op.
    pohunek_daemon::notify::ready()?;

    // 10. Serve both transports under ONE shutdown signal. A small task awaits
    //     the OS signal once and cancels each server, including a remote
    //     supervisor that is waiting to retry its bind.
    let shutdown = CancellationToken::new();
    let shutdown_sessions = sessions.clone();
    let shutdown_token = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_sessions.begin_daemon_shutdown();
        shutdown_token.cancel();
    });

    let control_connections = ConnectionTasks::new("control_connections", &shutdown);
    control_connections.register_shutdown_step(&tasks);
    let remote_connections = ConnectionTasks::new("remote_connections", &shutdown);
    remote_connections.register_shutdown_step(&tasks);

    let unix_shutdown = shutdown.clone();
    let unix_serve = server.serve(control_connections, async move {
        unix_shutdown.cancelled().await;
    });
    let remote_shutdown_sessions = sessions.clone();
    let missing_cli = if overlay_config.from_file() {
        MissingCli::Retry
    } else {
        MissingCli::Disable
    };
    let remote_serve = serve_remote(
        remote_state,
        &registry,
        missing_cli,
        (shutdown, remote_connections),
        move || {
            remote_shutdown_sessions.begin_daemon_shutdown();
        },
    );
    let ((), remote_result) = tokio::join!(unix_serve, remote_serve);

    // 11. Stop the daemon's tasks in ordered phases, each inside its own bound
    //     (see `pohunek_daemon::shutdown`): in-flight creates commit or
    //     compensate (shutdown already refuses new ones), the producers (idle
    //     stop, retention, package commands) stop first, then the consumers of
    //     their events (notification projector, hook flush), and the attention
    //     coordinator last so any last defers/resolves they hand over are
    //     accepted, the registry stops its own background work and
    //     refuses a store write after that (a control connection still
    //     finishing an operation sees `registry_shut_down`, which is how a
    //     daemon that is exiting ends it), and the append-only event log
    //     flushes last so events buffered at shutdown are not lost. The report
    //     is logged by the run.
    drop(tasks.run(&sessions).await);

    remote_result?;
    info!("pohunekd stopped");
    Ok(())
}

fn env_bool(var: &str) -> Result<bool, DaemonError> {
    let Some(value) = std::env::var_os(var) else {
        return Ok(false);
    };
    let Some(value) = value.to_str() else {
        return Err(DaemonError::InvalidEnv {
            var: var.to_owned(),
            value: "<non-utf8>".to_owned(),
            expected: "true/false, 1/0, yes/no, or on/off",
        });
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => Err(DaemonError::InvalidEnv {
            var: var.to_owned(),
            value: value.to_owned(),
            expected: "true/false, 1/0, yes/no, or on/off",
        }),
    }
}

/// Prints `pohunekd <version>` for the installer.
fn print_version() -> ExitCode {
    use std::io::Write as _;

    match writeln!(std::io::stdout(), "pohunekd {DAEMON_VERSION}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pohunekd: failed to print the version: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Worker supervision selected at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SupervisionMode {
    /// Native jobs configured by the installed `service.toml` at this path.
    Native(PathBuf),
    /// Direct `pohunek-sessiond` children under the dev/test contract.
    Subprocess,
}

/// Parses `pohunekd [--service-config <absolute path>]`.
fn parse_service_config(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<PathBuf>, DaemonError> {
    let mut arguments = arguments.into_iter();
    let mut service_config = None;
    while let Some(argument) = arguments.next() {
        if argument != SERVICE_CONFIG_FLAG {
            return Err(DaemonError::Arguments(format!(
                "unexpected argument {}",
                argument.to_string_lossy()
            )));
        }
        let path = arguments.next().map(PathBuf::from).ok_or_else(|| {
            DaemonError::Arguments(format!("{SERVICE_CONFIG_FLAG} requires a path"))
        })?;
        if !path.is_absolute() {
            return Err(DaemonError::Arguments(format!(
                "{SERVICE_CONFIG_FLAG} must be an absolute path, got {}",
                path.display()
            )));
        }
        if service_config.replace(path).is_some() {
            return Err(DaemonError::Arguments(format!(
                "{SERVICE_CONFIG_FLAG} was given more than once"
            )));
        }
    }
    Ok(service_config)
}

/// Chooses exactly one supervision mode from the command line and
/// [`WORKER_LAUNCHER_ENV`].
fn select_supervision(
    service_config: Option<PathBuf>,
    launcher: Option<OsString>,
) -> Result<SupervisionMode, DaemonError> {
    let subprocess = match launcher {
        None => false,
        Some(value) if value == SUBPROCESS_LAUNCHER => true,
        Some(value) => {
            return Err(DaemonError::InvalidEnv {
                var: WORKER_LAUNCHER_ENV.to_owned(),
                value: value.to_string_lossy().into_owned(),
                expected: "subprocess (native supervision is selected by --service-config)",
            })
        }
    };
    match (service_config, subprocess) {
        (Some(path), false) => Ok(SupervisionMode::Native(path)),
        (None, true) => Ok(SupervisionMode::Subprocess),
        (Some(_), true) => Err(DaemonError::SupervisionMode {
            detail:
                "--service-config and POHUNEK_WORKER_LAUNCHER=subprocess are mutually exclusive",
        }),
        (None, false) => Err(DaemonError::SupervisionMode {
            detail: "no supervision mode was selected",
        }),
    }
}

/// Applies the validated host input timings to every new session.
///
/// The submit delay is recorded provider-neutrally: only the runtime's own
/// descriptor (its `submit_delay_configurable`) decides which runtimes it
/// applies to. This function names no runtime.
fn apply_input_timing(config: &mut SessionRegistryConfig, timing: InputTiming) {
    config.initial_input_startup_grace = timing.initial_startup_grace;
    config.host_submit_delay = Some(timing.submit_delay);
    config.input_write_stall_timeout = timing.write_stall_timeout;
}

/// Applies every registry setting `service.toml` carries to `config`.
fn apply_service_settings(config: &mut SessionRegistryConfig, service: &ServiceConfig) {
    config.worker_connect_deadline = service.deadlines().worker_connect;
    let shutdown = service.deadlines().fitted_shutdown();
    config.shutdown_timeout = shutdown.registry_shutdown;
    config.create_drain_timeout = shutdown.create_drain;
    apply_input_timing(config, service.input_timing());
    apply_wait_timing(config, service.wait_timing());
    config.hook_stderr_tail_bytes = service.hook_stderr_tail_bytes();
    config.hook_required = service.hook_required().to_vec();
    config.event_retention = service.event_retention();
    config.inventory_probe = service.inventory_probe();
    config.worker_journal_max_age = service.worker_journal_max_age();
}

/// Applies the validated `session.wait_turn` default and ceiling and the
/// overlay connection keepalive.
fn apply_wait_timing(config: &mut SessionRegistryConfig, timing: WaitTiming) {
    config.turn_wait_default = timing.turn_default;
    config.turn_wait_max = timing.turn_max;
    config.remote_keepalive = timing.keepalive;
}

/// Logs one warning per key of `service.toml` this version does not know.
///
/// The key path and line identify the key; the value is never logged because a
/// hand-edited file may hold a secret there.
fn warn_unknown_service_keys(service: &ServiceConfig, path: &Path) {
    for key in service.unknown_keys() {
        warn!(
            event = "service_config.unknown_key",
            service.config = %path.display(),
            key.path = %key.path,
            key.line = key.line,
            key.suggestion = key.suggestion.as_deref().unwrap_or(""),
            "service.toml holds a key this version does not know; it is ignored"
        );
    }
}

/// Builds the session registry with the worker supervisor of `mode`.
async fn build_session_registry(
    mut config: SessionRegistryConfig,
    paths: &Paths,
    mode: SupervisionMode,
) -> Result<(SessionRegistry, ShutdownPlan), DaemonError> {
    let bootstrap_environment = bootstrap_environment()?;
    let working_directory = worker_working_directory(&bootstrap_environment)?;
    let mut plan = ShutdownPlan::default();
    let (supervision, supervisor): (SupervisionConfig, Arc<dyn WorkerLauncher>) = match mode {
        SupervisionMode::Native(path) => {
            let service = ServiceConfig::load(&path)?;
            warn_unknown_service_keys(&service, &path);
            service.verify_installation(
                nix::unistd::Uid::effective().as_raw(),
                &paths.state_dir,
                &paths.runtime_dir,
            )?;
            let deadlines = service.deadlines();
            // Phase bounds that do not fit the exit timeout are shortened to
            // fit, so the event-log flush still ends before the service
            // manager's kill; the file stays valid.
            plan = ShutdownPlan::from_deadlines(&deadlines);
            if let Some(shortfall) = &plan.shortfall {
                warn!(%shortfall, "service.toml shutdown bounds do not fit the exit timeout");
            }
            apply_service_settings(&mut config, &service);
            let (supervisor, worker_logs) = native_supervisor(&service, paths).await?;
            info!(
                service.namespace = %service.namespace(),
                service.version = service.active_version(),
                "supervising workers as native jobs"
            );
            (
                SupervisionConfig {
                    namespace: service.namespace(),
                    worker_executable: service.worker_executable(),
                    service_config: Some(path),
                    daemon_socket: paths.socket.clone(),
                    worker_initialize: deadlines.worker_initialize,
                    worker_exit_timeout: deadlines.worker_exit_timeout,
                    environment_allowlist: service.environment_allowlist().to_vec(),
                    environment_source: EnvironmentSource::Process,
                    sweep_grace: service.sweep_grace(),
                    open_files: service.open_files(),
                    bootstrap_environment,
                    working_directory,
                    worker_logs,
                },
                supervisor,
            )
        }
        SupervisionMode::Subprocess => {
            config.worker_connect_deadline = DEV_WORKER_CONNECT;
            let namespace = Namespace::derive(
                nix::unistd::Uid::effective().as_raw(),
                &std::fs::canonicalize(&paths.state_dir)?,
                &std::fs::canonicalize(&paths.runtime_dir)?,
            );
            warn!(
                service.namespace = %namespace,
                "supervising workers as direct children (dev/test subprocess mode)"
            );
            (
                SupervisionConfig {
                    namespace,
                    worker_executable: resolve_worker_binary()?,
                    service_config: None,
                    daemon_socket: paths.socket.clone(),
                    worker_initialize: DEV_WORKER_INITIALIZE,
                    worker_exit_timeout: DEV_WORKER_EXIT_TIMEOUT,
                    environment_allowlist: pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
                        .iter()
                        .map(|pattern| (*pattern).to_owned())
                        .collect(),
                    environment_source: EnvironmentSource::Process,
                    sweep_grace: DEV_SWEEP_GRACE,
                    open_files: DEV_OPEN_FILES,
                    bootstrap_environment,
                    working_directory,
                    worker_logs: None,
                },
                Arc::new(SubprocessWorkerLauncher::new()),
            )
        }
    };
    config.supervision = Some(supervision);
    let sessions =
        SessionRegistry::new_production(config, supervisor).map_err(DaemonError::Reconcile)?;
    Ok((sessions, plan))
}

/// Validates the bootstrap environment every worker job carries and returns
/// its working directory, `HOME`.
///
/// Every job definition applies the same rules, so a value they reject would
/// only surface after readiness as a failed launch of every session; startup
/// refuses it instead.
fn worker_working_directory(
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf, DaemonError> {
    for (name, value) in environment {
        if let Err(error) = supervisor::validate_bootstrap_variable(name, value) {
            error!(env.name = %name, error = %error, "bootstrap variable is not usable in a worker job");
            return Err(DaemonError::InvalidEnv {
                var: name.clone(),
                value: value.clone(),
                expected: "an absolute normalized path without `.` or `..` segments",
            });
        }
    }
    let home =
        environment
            .get("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| DaemonError::MissingEnv {
                var: "HOME".to_owned(),
            })?;
    if supervisor::validate_working_directory(&home).is_err() || !home.is_dir() {
        return Err(DaemonError::InvalidEnv {
            var: "HOME".to_owned(),
            value: home.display().to_string(),
            expected: "an existing absolute normalized directory (the worker working directory)",
        });
    }
    Ok(home)
}

/// Captures the bootstrap variables this daemon was started with.
fn bootstrap_environment() -> Result<BTreeMap<String, String>, DaemonError> {
    BOOTSTRAP_VARIABLES
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (*name, value)))
        .map(|(name, value)| {
            value
                .into_string()
                .map(|value| (name.to_owned(), value))
                .map_err(|_non_utf8| DaemonError::InvalidEnv {
                    var: name.to_owned(),
                    value: "<non-utf8>".to_owned(),
                    expected: "a UTF-8 absolute path",
                })
        })
        .collect()
}

/// Native worker supervisor and the log naming its definitions must use.
type NativeSupervisor = (Arc<dyn WorkerLauncher>, Option<LogNaming>);

/// Connects the systemd user manager that runs workers as transient units.
///
/// Worker output goes to the journal, so definitions name no log files. One
/// D-Bus call is bounded by the configured `launchctl_command` deadline: both
/// bound a single request to the user's service manager, so the installation
/// keeps one knob instead of inventing a second value.
#[cfg(target_os = "linux")]
async fn native_supervisor(
    service: &ServiceConfig,
    _paths: &Paths,
) -> Result<NativeSupervisor, DaemonError> {
    let supervisor = pohunek_platform::supervisor::systemd::SystemdSupervisor::connect(
        service.namespace(),
        service.deadlines().launchctl_command,
    )
    .await?;
    Ok((Arc::new(supervisor), None))
}

/// Creates the launchd supervisor that runs workers as `gui/<uid>` jobs.
///
/// Definitions must name exactly the log files the backend removes on
/// retirement, so the naming comes from the backend itself.
#[cfg(target_os = "macos")]
#[expect(
    clippy::unused_async,
    reason = "the signature is shared with the systemd target, which connects to D-Bus"
)]
async fn native_supervisor(
    service: &ServiceConfig,
    paths: &Paths,
) -> Result<NativeSupervisor, DaemonError> {
    let supervisor = Arc::new(
        pohunek_platform::supervisor::launchd::LaunchdSupervisor::new(
            service.namespace(),
            service.uid(),
            paths.state_dir.join(pohunek_paths::LAUNCHD_SUBDIR),
            paths.log_dir.join(pohunek_paths::LAUNCHD_SUBDIR),
            service.deadlines().launchctl_command,
        )?,
    );
    let naming = Arc::clone(&supervisor);
    let worker_logs = LogNaming::new(move |key| naming.worker_logs(key));
    Ok((supervisor, Some(worker_logs)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("pohunekd supervises workers only through systemd (Linux) or launchd (macOS)");

/// Resolves the durable worker binary: [`WORKER_BIN_ENV`] when set, otherwise the
/// `pohunek-sessiond` co-located next to the running daemon executable.
fn resolve_worker_binary() -> Result<PathBuf, DaemonError> {
    let binary = match std::env::var_os(WORKER_BIN_ENV) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .map(|dir| dir.join(WORKER_BINARY_NAME))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "daemon executable has no parent directory to locate the worker binary",
                )
            })?,
    };
    if !binary.is_absolute() {
        return Err(DaemonError::InvalidEnv {
            var: WORKER_BIN_ENV.to_owned(),
            value: binary.display().to_string(),
            expected: "an absolute path to pohunek-sessiond",
        });
    }
    Ok(binary)
}

/// Opens the event log and starts one drain per event source, as event-log
/// tasks of `tasks`.
fn spawn_event_logs(
    tasks: &DaemonShutdown,
    events_dir: &std::path::Path,
    sessions: &SessionRegistry,
    notifications: &NotificationService,
) -> Result<(), DaemonError> {
    let log = Arc::new(
        EventLog::open(events_dir, sessions.event_retention()).map_err(|source| {
            DaemonError::Directory {
                path: events_dir.to_path_buf(),
                source,
            }
        })?,
    );
    for (name, events) in [
        ("session_event_log", sessions.subscribe()),
        ("notification_event_log", notifications.subscribe()),
    ] {
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let handle = spawn_drain(Arc::clone(&log), events, shutdown);
        tasks.adopt(name, Stage::EventLog, move || stop.cancel(), handle);
    }
    Ok(())
}

/// Repeatedly bind and serve one optional overlay TCP control listener.
///
/// A `NetBird` daemon may start after `pohunekd`, or temporarily emit an
/// incomplete status snapshot while it initializes. Retrying keeps the local
/// control plane available immediately while allowing remote discovery to
/// recover without an operator restart. An active listener is revalidated on a
/// fixed interval; changed addresses are rebound before the stale listener is
/// dropped, while unavailable provider state drops it fail-closed and retries.
async fn serve_remote_with_retry<F, Fut>(
    mut bind: F,
    (shutdown, connections): (CancellationToken, ConnectionTasks),
    initial_retry_interval: Duration,
    reconcile_interval: Duration,
) where
    F: FnMut(Option<std::net::SocketAddr>) -> Fut,
    Fut: Future<Output = RemoteBind>,
{
    let mut retry_interval = initial_retry_interval;
    let mut active: Option<RemoteServer> = None;
    loop {
        if let Some(remote) = active.take() {
            let current_addr = remote.local_addr();
            let remote_shutdown = shutdown.clone();
            let mut serving = Box::pin(remote.serve(connections.clone(), async move {
                remote_shutdown.cancelled().await;
            }));
            let transition = loop {
                tokio::select! {
                    () = &mut serving => return,
                    () = tokio::time::sleep(reconcile_interval) => {
                        let attempt = tokio::select! {
                            () = shutdown.cancelled() => return,
                            attempt = bind(Some(current_addr)) => attempt,
                        };
                        match attempt {
                            RemoteBind::Unchanged => {}
                            other => break other,
                        }
                    }
                }
            };
            drop(serving);
            match transition {
                RemoteBind::Bound(remote) => {
                    info!(old_addr = %current_addr, new_addr = %remote.local_addr(), "overlay listener address changed; rebound remote control listener");
                    active = Some(*remote);
                    retry_interval = initial_retry_interval;
                    continue;
                }
                RemoteBind::Retry => {}
                RemoteBind::Disabled => {
                    shutdown.cancelled().await;
                    return;
                }
                RemoteBind::Unchanged => unreachable!("unchanged transitions continue serving"),
            }
        } else {
            let attempt = tokio::select! {
                () = shutdown.cancelled() => return,
                attempt = bind(None) => attempt,
            };
            match attempt {
                RemoteBind::Bound(remote) => {
                    active = Some(*remote);
                    retry_interval = initial_retry_interval;
                    continue;
                }
                RemoteBind::Disabled => {
                    shutdown.cancelled().await;
                    return;
                }
                RemoteBind::Retry => {}
                RemoteBind::Unchanged => {
                    error!("remote bind reported unchanged without an active listener");
                }
            }
        }

        debug!(
            retry_after_secs = retry_interval.as_secs(),
            "remote control listener unavailable; retrying after delay"
        );
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(retry_interval) => {}
        }
        retry_interval = next_retry_interval(retry_interval);
    }
}

/// Increase a retry delay without exceeding the local-only polling ceiling.
#[must_use]
fn next_retry_interval(current: Duration) -> Duration {
    current
        .saturating_mul(2)
        .min(REMOTE_BIND_MAX_RETRY_INTERVAL)
}

/// Serve the remote transport when its port configuration is valid.
async fn serve_remote<F>(
    state: DaemonState,
    registry: &OverlayRegistry,
    missing_cli: MissingCli,
    (shutdown, connections): (CancellationToken, ConnectionTasks),
    begin_daemon_shutdown: F,
) -> Result<(), DaemonError>
where
    F: FnOnce(),
{
    let mut tasks = Vec::with_capacity(registry.entries().len());
    for configured in registry.entries() {
        let configured = configured.clone();
        let overlay_id = configured.id().clone();
        let state = state.clone();
        let shutdown = shutdown.clone();
        let connections = connections.clone();
        let task = tokio::spawn(async move {
            serve_remote_with_retry(
                |current_addr| {
                    bind_remote_server(
                        state.clone(),
                        configured.clone(),
                        missing_cli,
                        current_addr,
                        |addr| async move {
                            tokio::net::TcpListener::bind(addr)
                                .await
                                .map(OpenedListener::os_bound)
                        },
                    )
                },
                (shutdown, connections),
                REMOTE_BIND_INITIAL_RETRY_INTERVAL,
                REMOTE_LISTENER_RECONCILE_INTERVAL,
            )
            .await;
        });
        tasks.push((overlay_id, task));
    }

    supervise_remote_tasks(tasks, shutdown, begin_daemon_shutdown).await
}

/// Monitor overlay listener supervisors and stop the daemon on task failure.
async fn supervise_remote_tasks<F>(
    tasks: Vec<(OverlayId, JoinHandle<()>)>,
    shutdown: CancellationToken,
    begin_daemon_shutdown: F,
) -> Result<(), DaemonError>
where
    F: FnOnce(),
{
    if tasks.is_empty() {
        shutdown.cancelled().await;
        return Ok(());
    }

    let mut completions = tasks
        .into_iter()
        .map(|(overlay_id, task)| async move { (overlay_id, task.await) })
        .collect::<FuturesUnordered<_>>();
    let mut first_failure = None;
    let mut begin_daemon_shutdown = Some(begin_daemon_shutdown);

    while let Some((overlay_id, result)) = completions.next().await {
        let reason = match result {
            Ok(()) if shutdown.is_cancelled() => continue,
            Ok(()) => "supervisor exited before shutdown".to_owned(),
            Err(join_error) => join_error.to_string(),
        };
        let failure = DaemonError::RemoteListenerSupervisor {
            overlay: overlay_id.to_string(),
            reason,
        };
        error!(
            overlay = %overlay_id,
            error = %failure,
            "remote listener supervisor failed; shutting down daemon"
        );
        if first_failure.is_none() {
            first_failure = Some(failure);
            let begin_daemon_shutdown = begin_daemon_shutdown
                .take()
                .expect("daemon shutdown callback is available for the first failure");
            begin_daemon_shutdown();
            shutdown.cancel();
        }
    }

    first_failure.map_or(Ok(()), Err)
}

/// Bind one overlay's TCP control listener once.
///
/// Queries the overlay for its explicit listener address and binds a
/// [`RemoteServer`] on that overlay's configured port. A missing CLI follows
/// `missing_cli`; unavailable state, a missing self address, and bind failures
/// are retried by the caller. `open` supplies the listening socket for the
/// validated address; production binds the address with the OS.
async fn bind_remote_server<O, Fut>(
    state: DaemonState,
    configured: ConfiguredTransport,
    missing_cli: MissingCli,
    current_addr: Option<std::net::SocketAddr>,
    open: O,
) -> RemoteBind
where
    O: FnOnce(std::net::SocketAddr) -> Fut,
    Fut: Future<Output = std::io::Result<OpenedListener>>,
{
    let overlay_id = configured.id().clone();
    let port = configured.port();
    let transport = Arc::clone(configured.transport());
    let address = match transport.listener_addr().await {
        Ok(address) => address,
        Err(overlay::OverlayError::CliMissing(_)) => {
            return match missing_cli {
                MissingCli::Disable => {
                    info!(overlay = %overlay_id, port, "overlay CLI missing; listener disabled");
                    RemoteBind::Disabled
                }
                MissingCli::Retry => {
                    warn!(overlay = %overlay_id, port, "overlay CLI missing; serving local-only and retrying");
                    RemoteBind::Retry
                }
            };
        }
        Err(error) => {
            info!(overlay = %overlay_id, port, reason = %error, "overlay state unavailable; serving local-only");
            return RemoteBind::Retry;
        }
    };
    let addr = std::net::SocketAddr::new(address, port);
    if current_addr == Some(addr) {
        return RemoteBind::Unchanged;
    }

    match RemoteServer::bind_with(addr, state, transport.as_ref(), open).await {
        Ok(remote) => {
            info!(overlay = %overlay_id, addr = %remote.local_addr(), "serving control protocol over overlay");
            RemoteBind::Bound(Box::new(remote))
        }
        Err(err) => {
            // Fail-closed validation or an OS bind error: log and stay local-only
            // rather than aborting the whole daemon.
            warn!(reason = %err, "remote control listener not bound; serving local-only");
            RemoteBind::Retry
        }
    }
}

/// What a missing provider CLI means for one overlay listener.
#[derive(Debug, Clone, Copy)]
enum MissingCli {
    /// The overlay is the built-in default that nobody configured, so a host
    /// without its CLI stays local-only until the daemon restarts.
    Disable,
    /// The owner listed the overlay in `overlays.toml`, so its CLI is expected
    /// to appear and the listener keeps retrying.
    Retry,
}

/// Result of one remote-listener bind attempt.
enum RemoteBind {
    /// The listener is bound and ready to serve.
    Bound(Box<RemoteServer>),
    /// Current provider state still validates the active listener address.
    Unchanged,
    /// The condition may recover without restarting the daemon.
    Retry,
    /// The configuration cannot enable remote transport during this run.
    Disabled,
}

/// Create a directory (and parents) with mode 0700 if it does not exist.
fn ensure_private_dir(dir: &std::path::Path) -> Result<(), DaemonError> {
    pohunek_platform::filesystem::TrustedDir::open_or_create_absolute(dir, 0o700)
        .map(|_| ())
        .map_err(|source| DaemonError::Directory {
            path: dir.to_path_buf(),
            source: std::io::Error::other(source),
        })
}

/// Resolve when SIGINT or SIGTERM is received.
async fn shutdown_signal() {
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(err) => {
            error!(error = %err, "failed to install SIGINT handler");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            error!(error = %err, "failed to install SIGTERM handler");
            return;
        }
    };
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT"),
        _ = sigterm.recv() => info!("received SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_log_guard_flushes_when_a_panic_unwinds_out_of_main() {
        use super::{FlushLogs, LOG_GUARD};
        use std::sync::PoisonError;

        let (_writer, guard) = tracing_appender::non_blocking(std::io::sink());
        *LOG_GUARD.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard);

        let unwound = std::panic::catch_unwind(|| {
            let _flush_logs = FlushLogs;
            panic!("a panic escaping block_on");
        });

        assert!(unwound.is_err());
        assert!(
            LOG_GUARD
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "the guard was dropped while the panic unwound"
        );
    }

    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;

    use overlay::{
        BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
        OverlayFuture, OverlayId, OverlayTransport, ResolvedPeer,
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::sync::CancellationToken;

    use pohunek_daemon::api::{
        ConnectionTasks, DaemonState, HealthInfo, OpenedListener, RemoteServer,
    };
    use pohunek_daemon::governance::HostGovernanceService;
    use pohunek_daemon::session::SessionRegistry;
    use pohunek_daemon::DaemonError;
    use protocol::{method, Request, Response};
    use serde_json::Value;

    use super::{
        bind_remote_server, next_retry_interval, serve_remote_with_retry, supervise_remote_tasks,
        MissingCli, RemoteBind, REMOTE_BIND_MAX_RETRY_INTERVAL,
    };

    /// The connection tasks of a remote listener that stops with `shutdown`.
    fn remote_connections(shutdown: &CancellationToken) -> ConnectionTasks {
        ConnectionTasks::new("remote_connections", shutdown)
    }

    /// A `NetBird` registry on the default port that reads no configuration file
    /// and no environment variable.
    fn netbird_registry() -> overlay::OverlayRegistry {
        let transport = Arc::new(netbird::NetbirdTransport::new());
        let entry = ConfiguredTransport::new(transport, pohunek_client::DEFAULT_REMOTE_PORT)
            .expect("configured NetBird overlay");
        overlay::OverlayRegistry::new(vec![entry]).expect("NetBird registry")
    }

    async fn governance_service() -> (tempfile::TempDir, Arc<HostGovernanceService>) {
        let root = pohunek_test_support::tempdir().expect("create isolated host-governance root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make isolated host-governance root owner-private");
        let service = Arc::new(
            HostGovernanceService::open(root.path().join("state"))
                .await
                .expect("open real isolated host-governance service"),
        );
        (root, service)
    }

    #[test]
    fn remote_retry_interval_caps_at_configured_maximum() {
        assert_eq!(
            next_retry_interval(REMOTE_BIND_MAX_RETRY_INTERVAL),
            REMOTE_BIND_MAX_RETRY_INTERVAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remote_supervisor_serves_after_a_transient_bind_failure() {
        let (_state_root, governance) = governance_service().await;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback remote listener");
        let addr = listener
            .local_addr()
            .expect("read loopback listener address");
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::default(),
            governance,
            netbird_registry(),
            pohunek_daemon::api::OverlaySource::Default,
        );
        let mut remote = Some(pohunek_daemon::api::RemoteServer::from_listener(
            listener, state,
        ));
        let attempts = Arc::new(AtomicUsize::new(0));
        let bind_attempts = Arc::clone(&attempts);
        let shutdown = CancellationToken::new();
        let supervisor_shutdown = shutdown.clone();
        let retry_interval = Duration::from_secs(1);

        let supervisor = tokio::spawn(serve_remote_with_retry(
            move |current_addr| {
                if current_addr.is_some() {
                    return std::future::ready(RemoteBind::Unchanged);
                }
                let attempt = bind_attempts.fetch_add(1, Ordering::Relaxed);
                let result = if attempt == 0 {
                    RemoteBind::Retry
                } else {
                    RemoteBind::Bound(Box::new(remote.take().expect("only one successful bind")))
                };
                std::future::ready(result)
            },
            (supervisor_shutdown, remote_connections(&shutdown)),
            retry_interval,
            Duration::from_secs(30),
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(retry_interval).await;
        tokio::task::yield_now().await;
        let mut stream = TcpStream::connect(addr)
            .await
            .expect("remote listener accepts a connection after retry");
        let request = Request::new("health", method::DAEMON_HEALTH, Value::Null)
            .expect("valid remote health request");
        let request = serde_json::to_string(&request).expect("serialize remote health request");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write remote health request");
        stream
            .write_all(b"\n")
            .await
            .expect("terminate remote health request");
        let mut response = [0_u8; 1024];
        let read = stream
            .read(&mut response)
            .await
            .expect("read remote health response");
        let response = std::str::from_utf8(&response[..read]).expect("health response is UTF-8");
        let response: Response = serde_json::from_str(response).expect("parse health response");
        assert!(response.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        shutdown.cancel();
        supervisor
            .await
            .expect("remote supervisor exits after shutdown");
    }

    #[derive(Debug)]
    struct MutableListenerTransport {
        id: OverlayId,
        address: Arc<RwLock<std::net::IpAddr>>,
    }

    impl OverlayTransport for MutableListenerTransport {
        fn id(&self) -> &OverlayId {
            &self.id
        }

        fn validate_bind_addr(&self, addr: std::net::IpAddr) -> Result<(), BindAddrError> {
            if *self.address.read().expect("listener address read lock") == addr {
                Ok(())
            } else {
                Err(BindAddrError::NotMember(addr))
            }
        }

        fn listener_addr(&self) -> OverlayFuture<'_, std::net::IpAddr> {
            let address = *self.address.read().expect("listener address read lock");
            Box::pin(async move { Ok(address) })
        }

        fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
            let error = OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay: self.id.clone(),
            };
            Box::pin(async move { Err(error) })
        }

        fn resolve_peer_identity<'a>(
            &'a self,
            identity: &'a ExternalIdentity,
        ) -> OverlayFuture<'a, ResolvedPeer> {
            let error = OverlayError::HostUnknown {
                host: identity.value().to_owned(),
                overlay: self.id.clone(),
            };
            Box::pin(async move { Err(error) })
        }

        fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Overlay-range addresses the supervisor tests bind; the core refuses to
    /// bind loopback, so the opener answers with a loopback socket instead.
    const FIRST_OVERLAY_ADDRESS: &str = "100.64.0.1";
    const SECOND_OVERLAY_ADDRESS: &str = "100.64.0.2";

    /// A loopback address distinct from `127.0.0.1`. Linux routes all of
    /// `127.0.0.0/8` to `lo`; Darwin configures only `127.0.0.1` on `lo0`,
    /// so the IPv6 loopback is its second local address.
    #[cfg(not(target_os = "macos"))]
    const SECOND_LOOPBACK: &str = "127.0.0.2";
    #[cfg(target_os = "macos")]
    const SECOND_LOOPBACK: &str = "::1";

    /// Bind attempts made to find one port that is free on both loopback addresses.
    ///
    /// A port taken on the second address by an unrelated socket is the only
    /// failure; a fresh ephemeral port on the first address is independent, so a
    /// handful of attempts makes exhaustion practically impossible.
    const RESERVE_ATTEMPTS: usize = 64;

    /// Bind one port on both addresses and keep the listeners open.
    ///
    /// The returned listeners stay bound until the supervisor takes them, so no
    /// other process can claim the port in between.
    async fn reserve_port_on_both(
        first_ip: std::net::IpAddr,
        second_ip: std::net::IpAddr,
    ) -> (u16, HashMap<std::net::IpAddr, TcpListener>) {
        for _ in 0..RESERVE_ATTEMPTS {
            let first = TcpListener::bind((first_ip, 0))
                .await
                .expect("first reservation");
            let port = first
                .local_addr()
                .expect("first reservation address")
                .port();
            if let Ok(second) = TcpListener::bind((second_ip, port)).await {
                let reserved = HashMap::from([(first_ip, first), (second_ip, second)]);
                return (port, reserved);
            }
        }
        panic!("no port free on both loopback addresses after {RESERVE_ATTEMPTS} attempts");
    }

    /// The overlay-range bind target of the first supervisor-test address.
    fn overlay_target(port: u16) -> std::net::SocketAddr {
        (
            FIRST_OVERLAY_ADDRESS
                .parse::<std::net::IpAddr>()
                .expect("first overlay IP"),
            port,
        )
            .into()
    }

    /// Run `bind_with` for `first_ip:port` with an opener that returns `listener`.
    async fn bind_with_returned_listener(
        listener: TcpListener,
        wrap: fn(TcpListener) -> OpenedListener,
        requested: std::net::SocketAddr,
    ) -> Result<RemoteServer, DaemonError> {
        let (_state_root, governance) = governance_service().await;
        let transport = MutableListenerTransport {
            id: OverlayId::new("mutable").expect("overlay id"),
            address: Arc::new(RwLock::new(requested.ip())),
        };
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::default(),
            governance,
            netbird_registry(),
            pohunek_daemon::api::OverlaySource::Default,
        );
        RemoteServer::bind_with(requested, state, &transport, |_| async {
            Ok(wrap(listener))
        })
        .await
    }

    #[tokio::test]
    async fn bind_with_rejects_listener_on_a_different_ip() {
        let first_ip = "127.0.0.1".parse().expect("first IP");
        let second_ip = SECOND_LOOPBACK.parse().expect("second IP");
        let (port, mut reserved) = reserve_port_on_both(first_ip, second_ip).await;
        let wrong = reserved.remove(&second_ip).expect("second listener");
        let result =
            bind_with_returned_listener(wrong, OpenedListener::held, overlay_target(port)).await;
        match result {
            Err(DaemonError::OverlayBind { addr, .. }) => assert_eq!(addr, second_ip),
            Err(other) => panic!("expected OverlayBind, got: {other}"),
            Ok(_) => panic!("a listener on another IP must be rejected"),
        }
    }

    #[tokio::test]
    async fn bind_with_trusts_an_os_bound_listener() {
        let first_ip = "127.0.0.1".parse().expect("first IP");
        let second_ip = SECOND_LOOPBACK.parse().expect("second IP");
        let (port, mut reserved) = reserve_port_on_both(first_ip, second_ip).await;
        let other = reserved.remove(&second_ip).expect("second listener");
        let server =
            bind_with_returned_listener(other, OpenedListener::os_bound, overlay_target(port))
                .await
                .expect("an OS-bound listener is not compared with the requested address");
        assert_eq!(server.local_addr(), (second_ip, port).into());
    }

    #[tokio::test]
    async fn bind_with_rejects_listener_on_a_different_port() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().expect("loopback IP");
        let wrong = TcpListener::bind((ip, 0))
            .await
            .expect("ephemeral listener");
        let wrong_port = wrong.local_addr().expect("listener address").port();
        let requested_port = wrong_port.checked_add(1).unwrap_or(wrong_port - 1);
        let result = bind_with_returned_listener(
            wrong,
            OpenedListener::held,
            overlay_target(requested_port),
        )
        .await;
        match result {
            Err(DaemonError::OverlayBind { addr, reason }) => {
                assert_eq!(addr, ip);
                assert!(reason.contains(&wrong_port.to_string()), "reason: {reason}");
            }
            Err(other) => panic!("expected OverlayBind, got: {other}"),
            Ok(_) => panic!("a listener on another port must be rejected"),
        }
    }

    /// Send `daemon.health` to `addr` and require an OK protocol response.
    ///
    /// A bare TCP handshake is not proof of a serving daemon: a reserved but
    /// unserved listener completes it too. The reply proves the supervisor's
    /// accept loop owns the socket.
    async fn assert_serves_health(addr: std::net::SocketAddr) {
        pohunek_test_support::wait::guard("daemon.health over TCP", async {
            let stream = TcpStream::connect(addr)
                .await
                .expect("listener accepts connections");
            let mut reader = BufReader::new(stream);
            let request = protocol::Request::new(
                "health",
                protocol::method::DAEMON_HEALTH,
                serde_json::Value::Null,
            )
            .expect("valid health request");
            let mut line = serde_json::to_string(&request).expect("serialize health request");
            line.push('\n');
            reader
                .get_mut()
                .write_all(line.as_bytes())
                .await
                .expect("send health request");
            let mut reply = String::new();
            reader
                .read_line(&mut reply)
                .await
                .expect("read health response");
            let response: protocol::Response =
                serde_json::from_str(&reply).expect("parse health response");
            response.into_result().expect("daemon.health succeeds");
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn remote_supervisor_rebinds_when_listener_address_changes() {
        let (_state_root, governance) = governance_service().await;
        let first_ip = "127.0.0.1".parse().expect("first IP");
        let second_ip = SECOND_LOOPBACK.parse().expect("second IP");
        let (port, mut sockets) = reserve_port_on_both(first_ip, second_ip).await;
        // The core refuses loopback bind targets, so the supervisor binds
        // overlay-range addresses and the opener substitutes a loopback socket.
        let reserved = Arc::new(Mutex::new(HashMap::from([
            (
                FIRST_OVERLAY_ADDRESS
                    .parse::<std::net::IpAddr>()
                    .expect("first overlay IP"),
                sockets.remove(&first_ip).expect("first socket"),
            ),
            (
                SECOND_OVERLAY_ADDRESS.parse().expect("second overlay IP"),
                sockets.remove(&second_ip).expect("second socket"),
            ),
        ])));
        // Real socket I/O runs while the clock is paused; only the explicit
        // `advance` below may move virtual time.
        let inhibitor = pohunek_test_support::time::AutoAdvanceInhibitor::new();
        let address = Arc::new(RwLock::new(
            FIRST_OVERLAY_ADDRESS.parse().expect("first overlay IP"),
        ));
        let transport = Arc::new(MutableListenerTransport {
            id: OverlayId::new("mutable").expect("overlay id"),
            address: Arc::clone(&address),
        });
        let configured = ConfiguredTransport::new(transport, port).expect("configured transport");
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::default(),
            governance,
            netbird_registry(),
            pohunek_daemon::api::OverlaySource::Default,
        );
        let shutdown = CancellationToken::new();
        let supervisor_shutdown = shutdown.clone();
        let reconcile_interval = Duration::from_secs(1);
        let supervisor = tokio::spawn(serve_remote_with_retry(
            move |current_addr| {
                let reserved = Arc::clone(&reserved);
                bind_remote_server(
                    state.clone(),
                    configured.clone(),
                    MissingCli::Disable,
                    current_addr,
                    move |addr| async move {
                        reserved
                            .lock()
                            .expect("reserved listener lock")
                            .remove(&addr.ip())
                            .map(OpenedListener::os_bound)
                            .ok_or_else(|| {
                                std::io::Error::other("no reserved listener for address")
                            })
                    },
                )
            },
            (supervisor_shutdown, remote_connections(&shutdown)),
            Duration::from_secs(1),
            reconcile_interval,
        ));

        tokio::task::yield_now().await;
        assert_serves_health((first_ip, port).into()).await;

        *address.write().expect("listener address write lock") =
            SECOND_OVERLAY_ADDRESS.parse().expect("second overlay IP");
        tokio::time::advance(reconcile_interval).await;
        tokio::task::yield_now().await;
        assert_serves_health((second_ip, port).into()).await;
        TcpStream::connect((first_ip, port))
            .await
            .expect_err("stale listener no longer accepts connections");

        shutdown.cancel();
        supervisor.await.expect("supervisor exits after shutdown");
        inhibitor.release().await;
    }

    #[tokio::test]
    async fn remote_supervisor_panic_cancels_and_drains_siblings() {
        let shutdown = CancellationToken::new();
        let daemon_shutdown_started = Arc::new(AtomicBool::new(false));
        let daemon_shutdown_started_by_failure = Arc::clone(&daemon_shutdown_started);
        let sibling_shutdown = shutdown.clone();
        let sibling_stopped = Arc::new(AtomicBool::new(false));
        let sibling_stopped_by_task = Arc::clone(&sibling_stopped);
        let sibling = tokio::spawn(async move {
            sibling_shutdown.cancelled().await;
            sibling_stopped_by_task.store(true, Ordering::Relaxed);
        });
        let failed = tokio::spawn(async {
            panic!("simulated remote listener supervisor panic");
        });

        let result = supervise_remote_tasks(
            vec![
                (
                    OverlayId::new("healthy").expect("valid overlay id"),
                    sibling,
                ),
                (OverlayId::new("failed").expect("valid overlay id"), failed),
            ],
            shutdown.clone(),
            move || {
                daemon_shutdown_started_by_failure.store(true, Ordering::Relaxed);
            },
        )
        .await;

        assert!(shutdown.is_cancelled());
        assert!(daemon_shutdown_started.load(Ordering::Relaxed));
        assert!(sibling_stopped.load(Ordering::Relaxed));
        match result.expect_err("panic must fail the remote transport") {
            DaemonError::RemoteListenerSupervisor { overlay, reason } => {
                assert_eq!(overlay, "failed");
                assert!(reason.contains("panicked"), "unexpected reason: {reason}");
            }
            error => panic!("unexpected daemon error: {error}"),
        }
    }

    #[tokio::test]
    async fn remote_supervisor_clean_exit_before_shutdown_fails_closed() {
        let shutdown = CancellationToken::new();
        let exited = tokio::spawn(async {});

        let result = supervise_remote_tasks(
            vec![(OverlayId::new("exited").expect("valid overlay id"), exited)],
            shutdown.clone(),
            || {},
        )
        .await;

        assert!(shutdown.is_cancelled());
        match result.expect_err("early exit must fail the remote transport") {
            DaemonError::RemoteListenerSupervisor { overlay, reason } => {
                assert_eq!(overlay, "exited");
                assert_eq!(reason, "supervisor exited before shutdown");
            }
            error => panic!("unexpected daemon error: {error}"),
        }
    }

    #[tokio::test]
    async fn remote_supervisors_exit_cleanly_after_shutdown() {
        let shutdown = CancellationToken::new();
        let daemon_shutdown_started = Arc::new(AtomicBool::new(false));
        let daemon_shutdown_started_by_failure = Arc::clone(&daemon_shutdown_started);
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            task_shutdown.cancelled().await;
        });
        shutdown.cancel();

        supervise_remote_tasks(
            vec![(OverlayId::new("stopped").expect("valid overlay id"), task)],
            shutdown,
            move || {
                daemon_shutdown_started_by_failure.store(true, Ordering::Relaxed);
            },
        )
        .await
        .expect("shutdown completion is expected");
        assert!(!daemon_shutdown_started.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod supervision_tests {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    use pohunek_daemon::DaemonError;
    use pohunek_test_support::process_env::ProcessEnv;

    use super::{
        apply_service_settings, env_bool, parse_service_config, select_supervision,
        warn_unknown_service_keys, worker_working_directory, SupervisionMode,
        OBSERVE_EXTERNAL_AGENTS_ENV,
    };

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    /// Shared buffer a test subscriber writes its formatted events to.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log capture").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn the_daemon_warns_once_per_unknown_service_key_without_its_value() {
        use std::os::unix::fs::PermissionsExt as _;

        use pohunek_service_config::ServiceConfig;

        const SECRET: &str = "PH-DAEMON-sentinel-4a17-d2b8";
        let dir = pohunek_test_support::tempdir().expect("config directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod config directory");
        let file = dir.path().join(pohunek_service_config::FILE_NAME);
        let text = format!(
            "schema_version = 7\nprefix = \"/home/u/.local\"\nactive_version = \"0.35.2\"\n\
             future_top = \"{SECRET}\"\n\
             [namespace]\nuid = 1000\nstate_root = \"/home/u/.local/state/pohunek\"\n\
             runtime_root = \"/run/user/1000/pohunek\"\n\
             [deadlines]\nworker_connect_ms = 10000\nworker_initialize_ms = 45000\n\
             launchctl_command_ms = 10000\nworker_exit_timeout_ms = 30000\n\
             daemon_exit_timeout_ms = 30000\ndaemon_restart_throttle_ms = 5000\n\
             [environment]\nsearch_path = []\nallowlist = [\"PATH\"]\n\
             [sweep]\ngrace_ms = 5000\n\
             [input]\ninitial_startup_grace_ms = 5000\nsubmit_delay_ms = 150\n\
             write_stall_timeout_ms = 3000\n\
             [waits]\nturn_default_ms = 300000\nturn_max_ms = 1800000\n\
             [hooks]\nstderr_tail_bytes = 4096\n\
             [events]\nmax_file_bytes = 33554432\nmax_files = 8\nmax_age_days = 14\n\
             [limits]\nopen_files = 8192\nopen_file = \"{SECRET}\"\n"
        );
        std::fs::write(&file, text).expect("write service.toml");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod service.toml");
        let service = ServiceConfig::load(&file).expect("a newer-minor file loads");

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_unknown_service_keys(&service, &file);
        });

        let logged = String::from_utf8_lossy(&capture.0.lock().expect("log capture")).into_owned();
        assert_eq!(
            logged.matches("service_config.unknown_key").count(),
            2,
            "{logged}"
        );
        assert!(logged.contains("WARN"), "{logged}");
        assert!(logged.contains("future_top"), "{logged}");
        assert!(logged.contains("limits.open_file"), "{logged}");
        assert!(logged.contains("limits.open_files"), "{logged}");
        assert!(logged.contains(&file.display().to_string()), "{logged}");
        assert!(!logged.contains(SECRET), "{logged}");
    }

    #[test]
    fn service_toml_events_retention_reaches_the_registry_the_journal_opens_with() {
        use std::os::unix::fs::PermissionsExt as _;

        use pohunek_service_config::{EventRetention, ServiceConfig};

        let dir = pohunek_test_support::tempdir().expect("config directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod config directory");
        let file = dir.path().join(pohunek_service_config::FILE_NAME);
        let text = String::from(
            "schema_version = 7\nprefix = \"/home/u/.local\"\nactive_version = \"0.35.2\"\n\
             [namespace]\nuid = 1000\nstate_root = \"/home/u/.local/state/pohunek\"\n\
             runtime_root = \"/run/user/1000/pohunek\"\n\
             [deadlines]\nworker_connect_ms = 10000\nworker_initialize_ms = 45000\n\
             launchctl_command_ms = 10000\nworker_exit_timeout_ms = 30000\n\
             daemon_exit_timeout_ms = 30000\ndaemon_restart_throttle_ms = 5000\n\
             [environment]\nsearch_path = []\nallowlist = [\"PATH\"]\n\
             [sweep]\ngrace_ms = 5000\n\
             [input]\ninitial_startup_grace_ms = 5000\nsubmit_delay_ms = 150\n\
             write_stall_timeout_ms = 3000\n\
             [waits]\nturn_default_ms = 300000\nturn_max_ms = 1800000\n\
             [hooks]\nstderr_tail_bytes = 4096\n\
             [events]\nmax_file_bytes = 2097152\nmax_files = 3\nmax_age_days = 2\n\
             [limits]\nopen_files = 8192\n",
        );
        std::fs::write(&file, text).expect("write service.toml");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod service.toml");
        let service = ServiceConfig::load(&file).expect("the file loads");

        let mut config = pohunek_daemon::session::SessionRegistryConfig::default();
        apply_service_settings(&mut config, &service);
        let sessions = pohunek_daemon::session::SessionRegistry::new(config);

        let expected = EventRetention {
            max_file_bytes: 2_097_152,
            max_files: 3,
            max_age: std::time::Duration::from_hours(48),
        };
        assert_ne!(expected, pohunek_service_config::DEFAULT_EVENT_RETENTION);
        assert_eq!(sessions.event_retention(), expected);
    }

    #[test]
    fn service_toml_registry_shutdown_bounds_the_registry_shutdown_wait() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        use pohunek_service_config::ServiceConfig;

        let dir = pohunek_test_support::tempdir().expect("config directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod config directory");
        let file = dir.path().join(pohunek_service_config::FILE_NAME);
        let text = String::from(
            "schema_version = 7\nprefix = \"/home/u/.local\"\nactive_version = \"0.35.2\"\n\
             [namespace]\nuid = 1000\nstate_root = \"/home/u/.local/state/pohunek\"\n\
             runtime_root = \"/run/user/1000/pohunek\"\n\
             [deadlines]\nworker_connect_ms = 10000\nworker_initialize_ms = 45000\n\
             launchctl_command_ms = 10000\nworker_exit_timeout_ms = 30000\n\
             daemon_exit_timeout_ms = 30000\ndaemon_restart_throttle_ms = 5000\n\
             registry_shutdown_ms = 7000\n\
             [environment]\nsearch_path = []\nallowlist = [\"PATH\"]\n\
             [sweep]\ngrace_ms = 5000\n\
             [input]\ninitial_startup_grace_ms = 5000\nsubmit_delay_ms = 150\n\
             write_stall_timeout_ms = 3000\n\
             [waits]\nturn_default_ms = 300000\nturn_max_ms = 1800000\n\
             [hooks]\nstderr_tail_bytes = 4096\n\
             [events]\nmax_file_bytes = 33554432\nmax_files = 8\nmax_age_days = 14\n\
             [limits]\nopen_files = 8192\n",
        );
        std::fs::write(&file, text).expect("write service.toml");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod service.toml");
        let service = ServiceConfig::load(&file).expect("the file loads");

        let mut config = pohunek_daemon::session::SessionRegistryConfig::default();
        apply_service_settings(&mut config, &service);

        let expected = Duration::from_secs(7);
        assert_ne!(expected, pohunek_service_config::DEFAULT_REGISTRY_SHUTDOWN);
        assert_eq!(config.shutdown_timeout, expected);
    }

    #[test]
    fn service_toml_keepalive_reaches_the_registry_the_remote_server_reads() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        use pohunek_service_config::{KeepaliveTiming, ServiceConfig};

        let dir = pohunek_test_support::tempdir().expect("config directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod config directory");
        let file = dir.path().join(pohunek_service_config::FILE_NAME);
        let text = String::from(
            "schema_version = 7\nprefix = \"/home/u/.local\"\nactive_version = \"0.35.2\"\n\
             [namespace]\nuid = 1000\nstate_root = \"/home/u/.local/state/pohunek\"\n\
             runtime_root = \"/run/user/1000/pohunek\"\n\
             [deadlines]\nworker_connect_ms = 10000\nworker_initialize_ms = 45000\n\
             launchctl_command_ms = 10000\nworker_exit_timeout_ms = 30000\n\
             daemon_exit_timeout_ms = 30000\ndaemon_restart_throttle_ms = 5000\n\
             [environment]\nsearch_path = []\nallowlist = [\"PATH\"]\n\
             [sweep]\ngrace_ms = 5000\n\
             [input]\ninitial_startup_grace_ms = 5000\nsubmit_delay_ms = 150\n\
             write_stall_timeout_ms = 3000\n\
             [waits]\nturn_default_ms = 300000\nturn_max_ms = 1800000\n\
             keepalive_idle_ms = 45000\nkeepalive_interval_ms = 15000\nkeepalive_count = 5\n\
             [hooks]\nstderr_tail_bytes = 4096\n\
             [events]\nmax_file_bytes = 33554432\nmax_files = 8\nmax_age_days = 14\n\
             [limits]\nopen_files = 8192\n",
        );
        std::fs::write(&file, text).expect("write service.toml");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod service.toml");
        let service = ServiceConfig::load(&file).expect("the file loads");

        let mut config = pohunek_daemon::session::SessionRegistryConfig::default();
        apply_service_settings(&mut config, &service);
        let sessions = pohunek_daemon::session::SessionRegistry::new(config);

        let expected = KeepaliveTiming {
            idle: Duration::from_secs(45),
            interval: Duration::from_secs(15),
            count: 5,
        };
        assert_ne!(expected, pohunek_service_config::DEFAULT_KEEPALIVE_TIMING);
        assert_eq!(sessions.remote_keepalive(), expected);
    }

    #[test]
    fn the_command_line_accepts_only_one_absolute_service_config() {
        assert_eq!(parse_service_config(arguments(&[])).expect("no flag"), None);
        assert_eq!(
            parse_service_config(arguments(&["--service-config", "/etc/p/service.toml"]))
                .expect("absolute path"),
            Some(PathBuf::from("/etc/p/service.toml"))
        );
        for rejected in [
            &["--service-config"][..],
            &["--service-config", "service.toml"],
            &["--service-config", "/a.toml", "--service-config", "/b.toml"],
            &["--worker-launcher", "subprocess"],
        ] {
            assert!(
                matches!(
                    parse_service_config(arguments(rejected)),
                    Err(DaemonError::Arguments(_))
                ),
                "{rejected:?} must be rejected"
            );
        }
    }

    #[test]
    fn exactly_one_supervision_mode_must_be_selected() {
        let config = PathBuf::from("/etc/p/service.toml");
        assert_eq!(
            select_supervision(Some(config.clone()), None).expect("native"),
            SupervisionMode::Native(config.clone())
        );
        assert_eq!(
            select_supervision(None, Some(OsString::from("subprocess"))).expect("dev/test"),
            SupervisionMode::Subprocess
        );
        for (service_config, launcher) in [
            (None, None),
            (Some(config.clone()), Some(OsString::from("subprocess"))),
        ] {
            let error = select_supervision(service_config, launcher)
                .expect_err("ambiguous or missing supervision");
            let message = error.to_string();
            assert!(matches!(error, DaemonError::SupervisionMode { .. }));
            assert!(
                message.contains("--service-config")
                    && message.contains("POHUNEK_WORKER_LAUNCHER=subprocess"),
                "the error names both options: {message}"
            );
        }
        assert!(matches!(
            select_supervision(None, Some(OsString::from("systemd"))),
            Err(DaemonError::InvalidEnv { .. })
        ));
    }

    #[test]
    fn external_agent_observation_is_opt_in_and_rejects_unknown_spellings() {
        use std::os::unix::ffi::OsStrExt as _;

        // The variable name and the spellings are the documented contract in
        // `docs/public-api.md` ("Daemon Runtime Configuration").
        const DOCUMENTED_NAME: &str = "POHUNEK_OBSERVE_EXTERNAL_AGENTS";

        if !pohunek_test_support::process_env::in_own_process() {
            return;
        }
        let mut env = ProcessEnv::lock();

        env.remove(DOCUMENTED_NAME);
        assert!(
            !env_bool(OBSERVE_EXTERNAL_AGENTS_ENV).expect("unset is valid"),
            "unset must leave external observation off"
        );

        for (value, enabled) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("off", false),
        ] {
            env.set(DOCUMENTED_NAME, value);
            assert_eq!(
                env_bool(OBSERVE_EXTERNAL_AGENTS_ENV).ok(),
                Some(enabled),
                "{DOCUMENTED_NAME}={value}"
            );
        }

        for (label, value) in [
            ("unknown word", OsStr::new("maybe")),
            ("non-UTF-8", OsStr::from_bytes(b"\xff")),
        ] {
            env.set(DOCUMENTED_NAME, value);
            let error = env_bool(OBSERVE_EXTERNAL_AGENTS_ENV).expect_err(label);
            assert!(
                matches!(&error, DaemonError::InvalidEnv { var, .. } if var == DOCUMENTED_NAME),
                "{label}: {error}"
            );
        }
    }

    fn environment(entries: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn worker_bootstrap_environment_must_satisfy_the_job_definition_rules() {
        let home =
            pohunek_test_support::tempdir_with_prefix("ph-home-").expect("create home directory");
        let home = home.path().to_str().expect("utf-8 home").to_owned();
        let state = format!("{home}/.local/state");
        assert_eq!(
            worker_working_directory(&environment(&[("HOME", &home), ("XDG_STATE_HOME", &state)]))
                .expect("absolute normalized home"),
            PathBuf::from(&home)
        );

        // Absolute XDG roots resolve every daemon path while HOME stays
        // relative; every worker job would reject it after readiness.
        for (name, value) in [
            ("HOME", "."),
            ("HOME", "relative/home"),
            ("HOME", "/nonexistent-pohunek-home"),
        ] {
            let error = worker_working_directory(&environment(&[
                (name, value),
                ("XDG_STATE_HOME", &state),
            ]))
            .expect_err("unusable worker working directory");
            assert!(
                matches!(&error, DaemonError::InvalidEnv { var, .. } if var == "HOME"),
                "{value:?}: {error}"
            );
        }
        let error = worker_working_directory(&environment(&[
            ("HOME", &home),
            ("XDG_CACHE_HOME", &format!("{home}/./cache")),
        ]))
        .expect_err("non-normalized bootstrap variable");
        assert!(
            matches!(&error, DaemonError::InvalidEnv { var, .. } if var == "XDG_CACHE_HOME"),
            "{error}"
        );
        assert!(matches!(
            worker_working_directory(&environment(&[("XDG_STATE_HOME", &state)])),
            Err(DaemonError::MissingEnv { .. })
        ));
    }
}
