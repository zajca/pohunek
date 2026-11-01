//! Supervises worker generations and the daemon through the systemd user manager.
//!
//! [`SystemdSupervisor`] starts every worker generation as a transient
//! service `pohunek-<ns>-worker-<session-id>-<generation>.service` through
//! `StartTransientUnit` on the session bus, so no unit file or template is
//! installed per worker. The persistent units live in the user unit directory
//! and are enabled for login: [`SystemdDaemon`] owns the daemon service and the
//! sessions slice, and [`SystemdPluginUnits`] owns the workflow plugin units
//! `pohunek-<ns>-wf-<plugin-hash>-<kind>-<name>.service` (`<kind>` is `j` for
//! a periodic job, `s` for a service) and, for a job, the `.timer` of the same
//! stem.
//!
//! # Supported systemd
//!
//! The minimum supported systemd is **255** (Ubuntu 24.04, the CI runner), the
//! oldest version this backend is exercised against. A probe on that runner
//! accepted `ExecStartEx`, `Type`, `NotifyAccess`, `KillMode`, `SendSIGHUP`,
//! `Slice`, `Restart`, both timeouts, and `Environment` as transient
//! properties. The real-manager tests (`tests/systemd.rs`) read every
//! property below back from the running manager; they pass on systemd 261.
//!
//! # Worker unit properties
//!
//! | Property | D-Bus type | Value | Reason |
//! |----------|-----------|-------|--------|
//! | `Description` | `s` | `Pohunek session worker <session-id> generation <generation>` | Identifies the unit in `systemctl` output. |
//! | `ExecStartEx` | `a(sasas)` | one command: the absolute executable, argv (`argv[0]` = executable), flags `["no-env-expand"]` | Exact versioned argv; no shell and no `$VAR` expansion. |
//! | `Type` | `s` | `notify` | Activation completes on `READY=1`. |
//! | `NotifyAccess` | `s` | `main` | Only the worker itself may report readiness. |
//! | `KillMode` | `s` | `control-group` | Stopping kills every descendant, including detached ones. |
//! | `SendSIGHUP` | `b` | `true` | Shells and agents see a hangup as on terminal close. |
//! | `Slice` | `s` | `pohunek-<ns>-sessions.slice` | Groups the installation's workers for accounting. |
//! | `Restart` | `s` | `no` | A generation owns one PTY; recovery is an explicit new generation. |
//! | `TimeoutStartUSec` | `t` | definition start timeout | Bounds activation when readiness never arrives. |
//! | `TimeoutStopUSec` | `t` | definition exit timeout | `SIGTERM`-to-`SIGKILL` grace. |
//! | `Environment` | `as` | bootstrap `KEY=VALUE` pairs | XDG roots and `HOME` only; D-Bus needs no quoting. |
//! | `WorkingDirectory` | `s` | definition working directory | Deterministic start directory. |
//! | `LimitNOFILE` / `LimitNOFILESoft` | `t` | definition open-file limit | Same descriptor budget as the launchd backend. |
//! | `StandardOutput` / `StandardError` | `s` | `journal` | Worker stdio stays in the user journal. |
//!
//! systemd expands `$VAR`, `${VAR}`, and `$$` in every argv word of a plain
//! `ExecStart` when it spawns the process, so a path containing `$` would run
//! a command line that differs from the definition. The `no-env-expand` flag,
//! the D-Bus form of the `:` unit-file prefix (systemd 242), keeps argv
//! verbatim; the manager still reports the command through the `ExecStart`
//! property that observations read back.
//!
//! The call never waits for readiness: a `Type=notify` worker only becomes
//! ready after the daemon connects to its socket, so waiting here would
//! deadlock.
//!
//! # D-Bus errors
//!
//! Errors are classified by D-Bus error name only, never by message text:
//!
//! | Error name | Meaning here |
//! |------------|--------------|
//! | `org.freedesktop.systemd1.NoSuchUnit`, `org.freedesktop.DBus.Error.UnknownObject` | [`Error::NotFound`]; [`Error::Race`] while a unit is being observed |
//! | `org.freedesktop.systemd1.UnitExists` | [`Error::AlreadyRegistered`] |
//! | `org.freedesktop.DBus.Error.ServiceUnknown`, `org.freedesktop.DBus.Error.NameHasNoOwner`, `org.freedesktop.DBus.Error.NoServer`, `org.freedesktop.DBus.Error.Disconnected` | [`Error::Unavailable`] |
//! | `org.freedesktop.DBus.Error.NoReply`, `org.freedesktop.DBus.Error.Timeout`, `org.freedesktop.DBus.Error.TimedOut` | [`Error::Timeout`] |
//! | any other name | [`Error::Operation`] |
//!
//! # Observations
//!
//! An observation binds the unit's main process only when it provably runs
//! `ExecStart`. Unit state, main PID, and control group are read twice around
//! the process checks; any change between the reads is [`Error::Race`].
//!
//! | Unit state | Main process | Result |
//! |------------|--------------|--------|
//! | any | none (`MainPID` 0) | state, `process: None` |
//! | any | runs `ExecStart` (canonical executable and argv) | state, `process: Some` |
//! | `activating` | not yet `ExecStart`: the manager's pre-`execve` fork (procfs denies it) or another image | [`ServiceState::Starting`], `process: None` |
//! | any other | not `ExecStart` | [`Error::InvalidData`] |
//! | any | reused PID or outside the unit's control group | [`Error::Race`] |
//!
//! A starting unit whose process is not bound is present but not ready, so
//! callers watch it until its initialization deadline instead of treating it
//! as unavailable.
//!
//! # Exit evidence
//!
//! [`Supervisor::exit_evidence`] reads the verdict of a `failed` unit from the
//! service interface: `Result` (an open string such as `exit-code`, `signal`,
//! `core-dump`, `timeout`, `oom-kill`), and `ExecMainCode` / `ExecMainStatus`
//! (the `si_code` and the exit code or signal number of the main process).
//! `ExecMainCode` and `ExecMainStatus` keep the last run's values and describe
//! the current process of a running unit, so any state other than `failed`
//! yields `None`. A unit the manager does not hold is [`Error::NotFound`];
//! retiring a failed unit resets it and destroys the record. With the default
//! `OOMPolicy=stop` a kernel OOM kill inside the unit's cgroup ends it
//! `failed` with `Result=oom-kill`, and `ExecMainStatus` then shows the signal
//! that stopped the main process, not the OOM, which is why the verdict comes
//! from `Result`.
//!
//! The same read, inside the `InvocationID` bracket, adds optional detail from
//! the service interface: `OOMKills` and `ManagedOOMKills` (systemd 259+;
//! processes the kernel and `systemd-oomd` killed), `MemoryPeak` (systemd 255+),
//! and `ExecMainExitTimestamp`. Each is read on its own: an unknown property
//! or the manager's `UINT64_MAX` "not available" leaves it absent without
//! losing the verdict. The counters and the peak live in the unit's runtime
//! record, so they are readable only until the failed unit is reset.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use zbus::proxy::CacheProperties;
use zbus::{OwnedObjectPath, Variant};

use super::{
    Activation, DaemonSupervisor, DefinitionFacts, Error, ExitEvidence, JobDefinition, JobExit,
    Namespace, Operation, PluginUnitDefinition, PluginUnitKey, PluginUnitKind, PluginUnits,
    RestartPolicy, ServiceId, ServiceObservation, ServiceState, Supervisor, UnitEnvironment,
    WorkerKey, MAX_JOB_TIMEOUT,
};
use crate::filesystem::{EntryKind, FsError, StageOutcome, TrustedDir};
use crate::process::{Error as ProcessError, LinuxInspector, ProcessIdentity, ProcessInspector};

mod unit_file;

#[doc(inline)]
pub use unit_file::{render_daemon_unit, render_sessions_slice};
use unit_file::{render_plugin_service, render_plugin_timer};

// Rust guideline compliant 2026-10-12

const DESTINATION: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
/// Property of the manager interface listing its environment as `KEY=VALUE`.
const MANAGER_ENVIRONMENT_PROPERTY: &str = "Environment";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";

/// Job mode for starting a unit.
///
/// `fail` refuses to reverse a queued stop job, so a start never silently
/// cancels a concurrent retirement.
const START_MODE: &str = "fail";
/// Job mode for stopping a unit.
///
/// `replace` cancels a queued start job. A `Type=notify` worker keeps its start
/// job until `READY=1`, and `fail` rejects that stop as a destructive
/// transaction (`org.freedesktop.systemd1.TransactionIsDestructive`, observed
/// on systemd 261), which would make a never-ready generation unretirable.
const STOP_MODE: &str = "replace";
/// Job mode for restarting the daemon with a replaced definition.
///
/// `replace` supersedes any queued start or stop of the daemon so the new
/// definition takes effect.
const RESTART_MODE: &str = "replace";
/// `ExecStartEx` flag that passes argv to the process without `$` expansion.
const NO_ENV_EXPAND: &str = "no-env-expand";

/// Bounds memory and D-Bus follow-up work if the manager returns an unexpectedly
/// large namespace.
const MAX_DISCOVERED_WORKERS: usize = 4_096;

/// Stop phases each bounded by `TimeoutStopUSec`.
///
/// systemd waits `TimeoutStopUSec` after `SIGTERM`, sends `SIGKILL`, and waits
/// up to `TimeoutStopUSec` again before it gives up on the control group.
const STOP_PHASES: u32 = 2;
/// Extra time allowed after both stop phases for the manager to record the
/// final state and for polling granularity.
const SETTLE_MARGIN: Duration = Duration::from_secs(5);
/// Delay between checks while waiting for a stopped unit to settle.
///
/// Short enough to return promptly after the last process exits, long enough
/// to keep a 60 s worst-case wait to about a thousand small D-Bus calls.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Mode of written unit files; systemd requires them to be readable, not
/// private.
const UNIT_FILE_MODE: u32 = 0o644;
/// Mode for a user unit directory this backend has to create.
const UNIT_DIR_MODE: u32 = 0o755;
/// Mode bits that make an existing unit directory unsafe to write into.
const UNIT_DIR_FORBIDDEN_BITS: u32 = 0o022;
/// Prefix of the name a unit file is moved to before it is removed.
///
/// systemd ignores names without a unit suffix, so the staged file is never
/// loaded.
const REMOVAL_PREFIX: &str = ".pohunek-unit-removed-";
/// Random bytes in a temporary unit-file name.
const TEMPORARY_NAME_ENTROPY: usize = 8;

/// Longest `Result` string the backend accepts from the manager.
///
/// The longest value systemd 262 defines is `start-limit-hit` (15 bytes);
/// 32 leaves room for a newer value while keeping a hostile or corrupted
/// property from being copied into callers' logs and records.
const MAX_RESULT_BYTES: usize = 32;

/// `ExecMainCode` of a main process that exited (`CLD_EXITED`).
const MAIN_EXITED: i32 = 1;
/// `ExecMainCode` of a main process killed by a signal (`CLD_KILLED`).
const MAIN_KILLED: i32 = 2;
/// `ExecMainCode` of a main process that dumped core (`CLD_DUMPED`).
const MAIN_DUMPED: i32 = 3;

const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";
const UNIT_EXISTS: &str = "org.freedesktop.systemd1.UnitExists";
const UNKNOWN_PROPERTY: &str = "org.freedesktop.DBus.Error.UnknownProperty";
const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const NO_SERVER: &str = "org.freedesktop.DBus.Error.NoServer";
const DISCONNECTED: &str = "org.freedesktop.DBus.Error.Disconnected";
const NO_REPLY: &str = "org.freedesktop.DBus.Error.NoReply";
const TIMEOUT: &str = "org.freedesktop.DBus.Error.Timeout";
const TIMED_OUT: &str = "org.freedesktop.DBus.Error.TimedOut";

/// One entry of `ListUnitsByPatterns`: name, description, load state, active
/// state, sub state, following unit, object path, job id, job type, job path.
type ListedUnit = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// One `ExecStart` entry: path, argv, ignore-failure flag, start and exit
/// timestamps (realtime, monotonic), PID, exit code, and exit status.
type ExecCommand = (String, Vec<String>, bool, u64, u64, u64, u64, u32, i32, i32);

/// One process of a unit's control group: cgroup path, PID, command line.
type UnitProcess = (String, u32, String);

/// Worker generations of one installation, run as transient user units.
#[derive(Debug, Clone)]
pub struct SystemdSupervisor {
    bus: Bus,
    namespace: Namespace,
}

/// Result of one namespace discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Observations of this namespace's worker units, sorted by service ID.
    pub observations: Vec<ServiceObservation>,
    /// Units matching the namespace pattern whose names do not parse strictly;
    /// they are never inspected or retired, and
    /// [`SystemdSupervisor::discover_strict`] refuses a result that lists any.
    pub rejected_units: Vec<String>,
    /// Whether a matching unit's observation raced, so the unit may be
    /// missing from [`Discovery::observations`]; only
    /// [`SystemdSupervisor::discover_strict`] refuses such a result.
    pub incomplete: bool,
}

impl SystemdSupervisor {
    /// Connects to the current user's session bus.
    ///
    /// Every D-Bus call on the connection fails with [`Error::Timeout`] after
    /// `call_timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unavailable`] when the session bus cannot be reached.
    pub async fn connect(namespace: Namespace, call_timeout: Duration) -> Result<Self, Error> {
        Ok(Self {
            bus: Bus::connect(call_timeout).await?,
            namespace,
        })
    }

    /// Returns the installation namespace this supervisor is bound to.
    #[must_use]
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// Discovers this namespace's worker units and reports rejected names.
    ///
    /// Units that vanish while being inspected are skipped: they are gone,
    /// so nothing can reference their version any more. A unit that changes
    /// generation during inspection marks the discovery
    /// [`Discovery::incomplete`] instead of being dropped, because the unit
    /// still exists and only [`SystemdSupervisor::discover_strict`] may
    /// decide what a caller that cannot tolerate an omission does.
    ///
    /// # Errors
    ///
    /// Returns a typed error when listing fails, more than 4096 units match,
    /// or a unit's inspection fails for a reason other than a race.
    pub async fn discover_units(&self) -> Result<Discovery, Error> {
        const OPERATION: &str = "discover";

        let manager = self.bus.manager(OPERATION).await?;
        let pattern = self.namespace.worker_unit_pattern();
        let units: Vec<ListedUnit> = manager
            .call(
                "ListUnitsByPatterns",
                &(Vec::<&str>::new(), vec![pattern.as_str()]),
            )
            .await
            .map_err(|source| bus_error(OPERATION, source))?;
        validate_discovery_count(units.len())?;
        let mut discovery = Discovery::default();
        for (name, ..) in units {
            match self.namespace.parse_worker_unit(&name) {
                Ok(key) => push_discovered_observation(
                    &mut discovery,
                    self.bus.observe(&name, &key.service_id()).await,
                )?,
                Err(_rejected) => discovery.rejected_units.push(name),
            }
        }
        discovery
            .observations
            .sort_by(|left, right| left.id.cmp(&right.id));
        Ok(discovery)
    }

    fn unit_name(&self, id: &ServiceId) -> Result<(WorkerKey, String), Error> {
        let key = WorkerKey::from_service_id(id)?;
        let name = self.namespace.worker_unit(&key);
        Ok((key, name))
    }
}

impl Supervisor for SystemdSupervisor {
    fn start<'a>(&'a self, id: &'a ServiceId, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "start";

            let (key, name) = self.unit_name(id)?;
            let properties = worker_properties(&self.namespace, &key, definition)?;
            let auxiliary: Vec<(&str, Vec<(&str, Variant<'_>)>)> = Vec::new();
            let manager = self.bus.manager(OPERATION).await?;
            let _job: OwnedObjectPath = manager
                .call(
                    "StartTransientUnit",
                    &(name.as_str(), START_MODE, properties, auxiliary),
                )
                .await
                .map_err(|source| unit_error(OPERATION, id, source))?;
            Ok(())
        })
    }

    /// Returns the lenient discovery for reconciliation.
    ///
    /// A raced unit is omitted, because discovery is repeated anyway; the
    /// destructive [`Supervisor::discover_strict`] refuses such a result
    /// instead.
    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move { Ok(lenient_discovery(self.discover_units().await?)) })
    }

    /// Returns the destructive discovery: a raced unit still exists but its
    /// identity is unproven, and a rejected unit of this namespace may still
    /// run, so a result that may omit either fails instead of letting a
    /// caller delete the version it may execute.
    fn discover_strict(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move { strict_discovery(self.discover_units().await?) })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation> {
        Box::pin(async move {
            let (_key, name) = self.unit_name(id)?;
            self.bus.observe(&name, id).await
        })
    }

    fn retire<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()> {
        Box::pin(async move {
            let (_key, name) = self.unit_name(id)?;
            self.bus.stop_and_settle(&name, id, "retire").await
        })
    }

    fn exit_status<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, Option<JobExit>> {
        Box::pin(async move {
            Ok(self
                .exit_evidence(id)
                .await?
                .and_then(|evidence| evidence.exit()))
        })
    }

    /// Reads the verdict of a `failed` unit.
    ///
    /// A transient unit that ended cleanly is collected, so only a failed
    /// unit keeps `Result`, `ExecMainCode` and `ExecMainStatus`; the last two
    /// also describe a running unit's current process, which is why any other
    /// state yields `None`. The reads are bracketed by the unit's state and
    /// invocation, and a change between them is a [`Error::Race`].
    fn exit_evidence<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, Option<ExitEvidence>> {
        Box::pin(async move {
            let (_key, name) = self.unit_name(id)?;
            self.bus.exit_evidence(&name, id).await
        })
    }
}

/// The persistent daemon unit and sessions slice of one installation.
///
/// Unit files live in the systemd user unit directory passed to
/// [`SystemdDaemon::connect`] (normally `$XDG_CONFIG_HOME/systemd/user`); the
/// user manager loads units only from its search path, so a private directory
/// would never be seen.
#[derive(Debug, Clone)]
pub struct SystemdDaemon {
    bus: Bus,
    namespace: Namespace,
    unit_dir: PathBuf,
    id: ServiceId,
}

impl SystemdDaemon {
    /// Connects to the current user's session bus for daemon management.
    ///
    /// `unit_dir` must be the absolute systemd user unit directory; it is
    /// created with mode `0755` when missing. Writes are refused when it or
    /// an ancestor owned by the user is group- or world-writable, because
    /// anyone able to swap that directory could plant units that run as the
    /// user. The daemon's observations use the daemon unit name as
    /// their service ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefinition`] for a relative `unit_dir` and
    /// [`Error::Unavailable`] when the session bus cannot be reached.
    pub async fn connect(
        namespace: Namespace,
        unit_dir: PathBuf,
        call_timeout: Duration,
    ) -> Result<Self, Error> {
        if !unit_dir.is_absolute() {
            return Err(Error::InvalidDefinition {
                detail: "the systemd user unit directory must be absolute".to_owned(),
            });
        }
        let id = ServiceId::parse(namespace.daemon_unit())?;
        Ok(Self {
            bus: Bus::connect(call_timeout).await?,
            namespace,
            unit_dir,
            id,
        })
    }

    /// Returns the systemd user unit directory receiving the unit files.
    #[must_use]
    pub fn unit_dir(&self) -> &Path {
        &self.unit_dir
    }

    /// Returns the service ID reported for the daemon unit.
    #[must_use]
    pub fn service_id(&self) -> &ServiceId {
        &self.id
    }

    async fn is_active(&self, name: &str) -> Result<bool, Error> {
        const OPERATION: &str = "inspect_daemon";

        let path = match self.bus.get_unit(name).await {
            Ok(path) => path,
            Err(source) if classify(&source) == Fault::Missing => return Ok(false),
            Err(source) => return Err(bus_error(OPERATION, source)),
        };
        let unit = self.bus.proxy(path, UNIT_INTERFACE, OPERATION).await?;
        match unit.get_property::<String>("ActiveState").await {
            Ok(state) => Ok(!is_ended(&state)),
            Err(source) if classify(&source) == Fault::Missing => Ok(false),
            Err(source) => Err(bus_error(OPERATION, source)),
        }
    }

    /// Enables the daemon unit for login; enabling an enabled unit is a no-op.
    ///
    /// The unit is enabled persistently (`runtime = false`) so the symlink
    /// survives reboot, and with `force = true` so a conflicting symlink is
    /// replaced rather than failing the call.
    async fn enable(
        &self,
        manager: &zbus::Proxy<'_>,
        name: &str,
        operation: &'static str,
    ) -> Result<(), Error> {
        let _changes: (bool, Vec<(String, String, String)>) = manager
            .call("EnableUnitFiles", &(vec![name], false, true))
            .await
            .map_err(|source| unit_error(operation, &self.id, source))?;
        Ok(())
    }

    fn write_units(&self, daemon_unit: &str, create: bool) -> Result<(), Error> {
        let Some(directory) = open_unit_dir(&self.unit_dir, create)? else {
            return Err(Error::NotFound(self.id.clone()));
        };
        write_unit(
            &directory,
            &self.namespace.sessions_slice(),
            &render_sessions_slice(),
        )?;
        write_unit(&directory, &self.namespace.daemon_unit(), daemon_unit)
    }
}

impl DaemonSupervisor for SystemdDaemon {
    fn install<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "install";

            validate_daemon_restart(definition)?;
            let unit = render_daemon_unit(definition)?;
            let name = self.namespace.daemon_unit();
            if self.is_active(&name).await? {
                return Err(Error::AlreadyRegistered(self.id.clone()));
            }
            self.write_units(&unit, true)?;
            self.bus.reload(OPERATION).await?;
            let manager = self.bus.manager(OPERATION).await?;
            self.enable(&manager, &name, OPERATION).await?;
            let _job: OwnedObjectPath = manager
                .call("StartUnit", &(name.as_str(), START_MODE))
                .await
                .map_err(|source| unit_error(OPERATION, &self.id, source))?;
            Ok(())
        })
    }

    fn replace<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "replace";

            validate_daemon_restart(definition)?;
            let unit = render_daemon_unit(definition)?;
            let name = self.namespace.daemon_unit();
            let present = open_unit_dir(&self.unit_dir, false)?
                .map(|directory| directory.entry_identity(&name, EntryKind::RegularFile))
                .transpose()
                .map_err(|source| fs_error("inspect_unit_file", source))?
                .flatten()
                .is_some();
            if !present {
                return Err(Error::NotFound(self.id.clone()));
            }
            self.write_units(&unit, false)?;
            self.bus.reload(OPERATION).await?;
            let manager = self.bus.manager(OPERATION).await?;
            // A resumed install reaches `replace` with the unit file written
            // but possibly never enabled, so enabling is part of the contract.
            self.enable(&manager, &name, OPERATION).await?;
            let _job: OwnedObjectPath = manager
                .call("RestartUnit", &(name.as_str(), RESTART_MODE))
                .await
                .map_err(|source| unit_error(OPERATION, &self.id, source))?;
            Ok(())
        })
    }

    fn inspect(&self) -> Operation<'_, ServiceObservation> {
        Box::pin(async move {
            self.bus
                .observe(&self.namespace.daemon_unit(), &self.id)
                .await
        })
    }

    fn uninstall(&self) -> Operation<'_, ()> {
        Box::pin(async move {
            const OPERATION: &str = "uninstall";

            let name = self.namespace.daemon_unit();
            match self.bus.stop_and_settle(&name, &self.id, OPERATION).await {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            let manager = self.bus.manager(OPERATION).await?;
            match manager
                .call::<_, _, Vec<(String, String, String)>>(
                    "DisableUnitFiles",
                    &(vec![name.as_str()], false),
                )
                .await
            {
                Ok(_changes) => {}
                Err(source) if classify(&source) == Fault::Missing => {}
                Err(source) => return Err(bus_error(OPERATION, source)),
            }
            if let Some(directory) = open_unit_dir(&self.unit_dir, false)? {
                remove_unit(&directory, &name)?;
                remove_unit(&directory, &self.namespace.sessions_slice())?;
            }
            self.bus.reload(OPERATION).await?;
            // The sessions slice and its implicit dash-hierarchy parent stay
            // active after their last worker exits. Stop them only when their
            // control groups are empty, so uninstall never touches a worker.
            for slice in [
                self.namespace.sessions_slice(),
                parent_slice(&self.namespace),
            ] {
                self.bus.stop_empty_slice(&slice, OPERATION).await?;
            }
            Ok(())
        })
    }
}

/// The persistent workflow plugin units of one installation.
///
/// Unit files live in the systemd user unit directory passed to
/// [`SystemdPluginUnits::connect`], next to the daemon unit. Errors about one
/// unit set report the service unit name as the service ID.
///
/// Both kinds use `KillMode=mixed`: a stop sends `SIGTERM` to the unit's main
/// process only, and `SIGKILL` to every remaining process once that process
/// ended or the stop timeout passed, so the main process can stop its own
/// descendants first.
///
/// A periodic job is a `Type=oneshot` service started only by its enabled
/// timer. The timer's first run starts `every` after the timer is activated
/// (`OnActiveSec=`) and each later run `every` after the previous run ended
/// (`OnUnitInactiveSec=`), both on the monotonic clock, which pauses during
/// suspend; activation happens again at every start of the user manager. The
/// job's start timeout is its `TimeoutStartSec=`, which bounds the whole run of
/// a `Type=oneshot` service. A long-running service is a `Type=exec` service
/// enabled for login with `Restart=always` and `RestartSec=` set to the delay,
/// which systemd waits after each exit.
#[derive(Debug, Clone)]
pub struct SystemdPluginUnits {
    bus: Bus,
    namespace: Namespace,
    unit_dir: PathBuf,
}

/// The unit names of one plugin unit set.
#[derive(Debug, Clone)]
struct PluginUnitNames {
    service: String,
    /// The timer of a job; a service has none.
    timer: Option<String>,
    id: ServiceId,
}

impl PluginUnitNames {
    /// Returns the unit that activates the set: a job's timer or the service.
    fn activation(&self) -> &str {
        self.timer.as_deref().unwrap_or(&self.service)
    }

    /// Returns every unit of the set, the timer first.
    fn units(&self) -> impl Iterator<Item = &str> {
        self.timer
            .as_deref()
            .into_iter()
            .chain(std::iter::once(self.service.as_str()))
    }
}

impl SystemdPluginUnits {
    /// Connects to the current user's session bus for plugin unit management.
    ///
    /// `unit_dir` must be the absolute systemd user unit directory; it is
    /// created with mode `0755` when missing, and writes are refused when it or
    /// an ancestor owned by the user is group- or world-writable, as for
    /// [`SystemdDaemon::connect`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefinition`] for a relative `unit_dir` and
    /// [`Error::Unavailable`] when the session bus cannot be reached.
    pub async fn connect(
        namespace: Namespace,
        unit_dir: PathBuf,
        call_timeout: Duration,
    ) -> Result<Self, Error> {
        if !unit_dir.is_absolute() {
            return Err(Error::InvalidDefinition {
                detail: "the systemd user unit directory must be absolute".to_owned(),
            });
        }
        Ok(Self {
            bus: Bus::connect(call_timeout).await?,
            namespace,
            unit_dir,
        })
    }

    fn names(&self, key: &PluginUnitKey) -> PluginUnitNames {
        let service = self.namespace.plugin_service_unit(key);
        let id = ServiceId::parse(service.as_str())
            .expect("plugin unit names are bounded and use the service ID alphabet");
        PluginUnitNames {
            timer: (key.kind() == PluginUnitKind::Job)
                .then(|| self.namespace.plugin_timer_unit(key)),
            service,
            id,
        }
    }

    /// Reports whether any unit file of the set exists.
    fn installed(&self, names: &PluginUnitNames) -> Result<bool, Error> {
        const OPERATION: &str = "inspect_unit_file";

        let Some(directory) = open_unit_dir(&self.unit_dir, false)? else {
            return Ok(false);
        };
        for name in names.units() {
            if directory
                .entry_identity(name, EntryKind::RegularFile)
                .map_err(|source| fs_error(OPERATION, source))?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Stops the timer, then the service, and waits until both ended.
    ///
    /// The timer goes first so it cannot start a new run in between. Units
    /// the manager has not loaded are already stopped.
    async fn stop_set(
        &self,
        names: &PluginUnitNames,
        operation: &'static str,
    ) -> Result<(), Error> {
        if let Some(timer) = &names.timer {
            self.bus.stop_timer(timer, operation).await?;
        }
        match self
            .bus
            .stop_and_settle(&names.service, &names.id, operation)
            .await
        {
            Ok(()) | Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl PluginUnits for SystemdPluginUnits {
    fn install<'a>(
        &'a self,
        key: &'a PluginUnitKey,
        definition: &'a PluginUnitDefinition,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "install_plugin_unit";

            if definition.kind() != key.kind() {
                return Err(Error::InvalidDefinition {
                    detail: "the definition's kind differs from the unit key's".to_owned(),
                });
            }
            let names = self.names(key);
            let service = render_plugin_service(key, definition)?;
            let timer = match (definition.activation(), &names.timer) {
                (Activation::Interval { every, accuracy }, Some(timer)) => Some((
                    timer,
                    render_plugin_timer(key, &names.service, every, accuracy),
                )),
                _ => None,
            };
            let directory = open_unit_dir(&self.unit_dir, true)?
                .ok_or_else(|| Error::NotFound(names.id.clone()))?;
            // A replaced set is stopped first, so no process or run of the
            // previous definition outlives the rewrite.
            self.stop_set(&names, OPERATION).await?;
            write_unit(&directory, &names.service, &service)?;
            if let Some((name, contents)) = &timer {
                write_unit(&directory, name, contents)?;
            }
            self.bus.reload(OPERATION).await?;
            let manager = self.bus.manager(OPERATION).await?;
            let _changes: (bool, Vec<(String, String, String)>) = manager
                .call("EnableUnitFiles", &(vec![names.activation()], false, true))
                .await
                .map_err(|source| unit_error(OPERATION, &names.id, source))?;
            let _job: OwnedObjectPath = manager
                .call("StartUnit", &(names.activation(), START_MODE))
                .await
                .map_err(|source| unit_error(OPERATION, &names.id, source))?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "start_plugin_unit";

            let names = self.names(key);
            if !self.installed(&names)? {
                return Err(Error::NotFound(names.id));
            }
            let _job: OwnedObjectPath = self
                .bus
                .manager(OPERATION)
                .await?
                .call("StartUnit", &(names.activation(), START_MODE))
                .await
                .map_err(|source| unit_error(OPERATION, &names.id, source))?;
            Ok(())
        })
    }

    fn stop<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "stop_plugin_unit";

            let names = self.names(key);
            if !self.installed(&names)? {
                return Err(Error::NotFound(names.id));
            }
            self.stop_set(&names, OPERATION).await
        })
    }

    fn remove<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ()> {
        Box::pin(async move {
            const OPERATION: &str = "remove_plugin_unit";

            let names = self.names(key);
            self.stop_set(&names, OPERATION).await?;
            let manager = self.bus.manager(OPERATION).await?;
            for name in names.units() {
                match manager
                    .call::<_, _, Vec<(String, String, String)>>(
                        "DisableUnitFiles",
                        &(vec![name], false),
                    )
                    .await
                {
                    Ok(_changes) => {}
                    Err(source) if classify(&source) == Fault::Missing => {}
                    Err(source) => return Err(bus_error(OPERATION, source)),
                }
            }
            if let Some(directory) = open_unit_dir(&self.unit_dir, false)? {
                for name in names.units() {
                    remove_unit(&directory, name)?;
                }
            }
            self.bus.reload(OPERATION).await?;
            // A unit left failed stays loaded after its file is gone until
            // its failure is reset.
            for name in names.units() {
                match manager.call::<_, _, ()>("ResetFailedUnit", &(name,)).await {
                    Ok(()) => {}
                    Err(source) if classify(&source) == Fault::Missing => {}
                    Err(source) => return Err(bus_error(OPERATION, source)),
                }
            }
            Ok(())
        })
    }

    fn state<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ServiceState> {
        Box::pin(async move {
            const OPERATION: &str = "plugin_unit_state";

            let names = self.names(key);
            if !self.installed(&names)? {
                return Err(Error::NotFound(names.id));
            }
            let service = self.bus.active_state(&names.service, OPERATION).await?;
            let Some(timer) = &names.timer else {
                // An installed service the manager has not loaded is inactive.
                return Ok(service.map_or(ServiceState::Stopped, |(active, sub)| {
                    portable_state(&active, &sub)
                }));
            };
            let timer = self.bus.active_state(timer, OPERATION).await?;
            let is = |state: &Option<(String, String)>, expected: &str| {
                state.as_ref().is_some_and(|(active, _)| active == expected)
            };
            Ok(if is(&service, "failed") || is(&timer, "failed") {
                ServiceState::Failed
            } else if is(&timer, "active") {
                ServiceState::Running
            } else {
                ServiceState::Stopped
            })
        })
    }
}

/// Requires the daemon to restart only after a failure, as the launchd daemon
/// agent does: a never-restarting daemon stays down after a crash, and one
/// restarting after a clean exit could never be stopped by its own shutdown.
fn validate_daemon_restart(definition: &JobDefinition) -> Result<(), Error> {
    match definition.restart() {
        RestartPolicy::OnFailure { .. } => Ok(()),
        RestartPolicy::Never => Err(Error::InvalidDefinition {
            detail: "the daemon unit must restart after a failure".to_owned(),
        }),
        RestartPolicy::Always { .. } => Err(Error::InvalidDefinition {
            detail: "the daemon unit must stay stopped after a clean exit".to_owned(),
        }),
    }
}

/// Returns the implicit parent of the sessions slice.
///
/// systemd derives slice nesting from dashes, so `pohunek-<ns>-sessions.slice`
/// lives below `pohunek-<ns>.slice`, which lives below `pohunek.slice`. The
/// shared `pohunek.slice` may hold other installations and is never stopped.
fn parent_slice(namespace: &Namespace) -> String {
    format!("pohunek-{}.slice", namespace.as_str())
}

/// Reads the `Environment` property of the systemd user manager.
///
/// The property is the `KEY=VALUE` list the manager gives its units. A unit
/// that records no `PATH` runs with that list's `PATH`, so a caller that must
/// judge with the same `PATH` reads it here. The session bus connection and
/// the call are both bounded by `call_timeout`.
///
/// # Errors
///
/// Returns [`Error::Unavailable`] when the session bus cannot be reached,
/// [`Error::Timeout`] when a call does not finish within `call_timeout`, and
/// [`Error::Operation`] when the manager rejects the property read.
pub async fn user_manager_environment(call_timeout: Duration) -> Result<Vec<String>, Error> {
    read_user_manager_environment(None, call_timeout).await
}

/// [`user_manager_environment`] over the bus at `address`, or over the session
/// bus the environment names when `address` is `None`.
async fn read_user_manager_environment(
    address: Option<&str>,
    call_timeout: Duration,
) -> Result<Vec<String>, Error> {
    const OPERATION: &str = "read_user_manager_environment";

    let bus = Bus::connect_to(address, call_timeout).await?;
    let manager = bus.manager(OPERATION).await?;
    manager
        .get_property::<Vec<String>>(MANAGER_ENVIRONMENT_PROPERTY)
        .await
        .map_err(|source| bus_error(OPERATION, source))
}

/// Reads the environment configuration the user manager holds for `unit`.
///
/// The unit is loaded (not started) so drop-ins apply. A unit with no file and
/// no drop-in reports `not-found` and empty settings, which is the state of a
/// job that is not installed yet. Both the connection and the calls are bounded
/// by `call_timeout`.
///
/// # Errors
///
/// Returns [`Error::Unavailable`] when the session bus cannot be reached,
/// [`Error::Timeout`] when a call does not finish within `call_timeout`, and
/// [`Error::Operation`] when the manager rejects a read.
pub async fn unit_environment(
    unit: &str,
    call_timeout: Duration,
) -> Result<UnitEnvironment, Error> {
    read_unit_environment(None, unit, call_timeout).await
}

/// The `*.conf` drop-in files on disk that apply to `unit` in the manager's own
/// lookup directories (`unit_paths`, from its `UnitPath` property). Each lookup
/// directory is listed in turn, higher priority first, so of same-named `.d`
/// directories the higher-priority one comes first.
///
/// A unit `a-b-c.service` takes drop-ins from `a-b-c.service.d`, then the
/// dash-truncated `a-b-.service.d` and `a-.service.d`, then `service.d`. A
/// directory that is absent has none; any other failure to read one is an error,
/// because a guess of "none" would let an override through.
fn drop_in_files_on_disk(unit: &str, unit_paths: &[String]) -> Result<Vec<String>, Error> {
    const OPERATION: &str = "read_unit_drop_ins";

    let stem = unit.strip_suffix(".service").unwrap_or(unit);
    let mut names = vec![unit.to_owned()];
    let mut prefix = stem;
    while let Some(dash) = prefix.rfind('-') {
        prefix = &prefix[..dash];
        names.push(format!("{prefix}-.service"));
    }
    names.push("service".to_owned());

    let failed = |source: io::Error| Error::Operation {
        operation: OPERATION,
        source: Box::new(source),
    };
    let mut files = Vec::new();
    for directory in unit_paths {
        for name in &names {
            let entries = match std::fs::read_dir(Path::new(directory).join(format!("{name}.d"))) {
                Ok(entries) => entries,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(failed(error)),
            };
            for entry in entries {
                let entry = entry.map_err(failed)?;
                let path = entry.path();
                if !hidden_file(&entry.file_name())
                    && path.extension().is_some_and(|ext| ext == "conf")
                {
                    files.push(path.display().to_string());
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|file| seen.insert(file.clone()));
    Ok(files)
}

/// Whether systemd skips a directory entry as hidden: a name that starts with a
/// dot (an editor lock or a dotfile). The other names it skips as backups (a `~`
/// suffix, package-manager suffixes) cannot end in `.conf`, which the caller
/// requires. The name is compared as bytes, because systemd does not require
/// UTF-8.
fn hidden_file(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;

    name.as_bytes().starts_with(b".")
}

/// [`unit_environment`] over the bus at `address`, or over the session bus the
/// environment names when `address` is `None`.
async fn read_unit_environment(
    address: Option<&str>,
    unit: &str,
    call_timeout: Duration,
) -> Result<UnitEnvironment, Error> {
    const OPERATION: &str = "read_unit_environment";

    let bus = Bus::connect_to(address, call_timeout).await?;
    let manager = bus.manager(OPERATION).await?;
    let read = |source| bus_error(OPERATION, source);
    let path: OwnedObjectPath = manager
        .call("LoadUnit", &(unit,))
        .await
        .map_err(|source| bus_error(OPERATION, source))?;
    let unit_paths: Vec<String> = manager.get_property("UnitPath").await.map_err(read)?;
    let unit_proxy = bus.proxy(path.clone(), UNIT_INTERFACE, OPERATION).await?;
    let service = bus.proxy(path, SERVICE_INTERFACE, OPERATION).await?;
    let load_state: String = unit_proxy.get_property("LoadState").await.map_err(read)?;
    let need_daemon_reload: bool = unit_proxy
        .get_property("NeedDaemonReload")
        .await
        .map_err(read)?;
    let drop_in_paths: Vec<String> = unit_proxy.get_property("DropInPaths").await.map_err(read)?;
    let fragment_path: String = unit_proxy
        .get_property("FragmentPath")
        .await
        .map_err(read)?;
    let drop_ins_on_disk = drop_in_files_on_disk(unit, &unit_paths)?;
    if load_state == "not-found" && drop_in_paths.is_empty() {
        return Ok(UnitEnvironment {
            load_state,
            need_daemon_reload,
            drop_ins_on_disk,
            ..UnitEnvironment::default()
        });
    }
    Ok(UnitEnvironment {
        load_state,
        need_daemon_reload,
        fragment_path,
        drop_ins_on_disk,
        drop_in_paths,
        environment: service.get_property("Environment").await.map_err(read)?,
        exec_search_path: service.get_property("ExecSearchPath").await.map_err(read)?,
        environment_files: service
            .get_property("EnvironmentFiles")
            .await
            .map_err(read)?,
        pass_environment: service
            .get_property("PassEnvironment")
            .await
            .map_err(read)?,
        unset_environment: service
            .get_property("UnsetEnvironment")
            .await
            .map_err(read)?,
    })
}

/// Session-bus connection to the systemd user manager.
#[derive(Debug, Clone)]
struct Bus {
    connection: zbus::Connection,
}

impl Bus {
    async fn connect(call_timeout: Duration) -> Result<Self, Error> {
        Self::connect_to(None, call_timeout).await
    }

    /// Connects to the bus at `address`, or to the session bus the environment
    /// names when `address` is `None`.
    async fn connect_to(address: Option<&str>, call_timeout: Duration) -> Result<Self, Error> {
        const OPERATION: &str = "connect";

        // An unusable address is reported by `build()`, which classifies it
        // as unavailable like every other connection failure.
        let builder = match address {
            Some(address) => zbus::connection::Builder::address(address),
            None => zbus::connection::Builder::session(),
        };
        let connection = builder
            .method_timeout(call_timeout)
            .build()
            .await
            .map_err(|source| {
                // The connect failure's text quotes the address, which the
                // caller's environment supplies; only the I/O cause is kept.
                let source = match source {
                    zbus::Error::Connection(io, _address) => zbus::Error::InputOutput(io),
                    other => other,
                };
                match classify(&source) {
                    Fault::TimedOut => Error::Timeout {
                        operation: OPERATION,
                    },
                    _ => unavailable(OPERATION, source),
                }
            })?;
        Ok(Self { connection })
    }

    async fn manager(&self, operation: &'static str) -> Result<zbus::Proxy<'_>, Error> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(DESTINATION)
            .path(MANAGER_PATH)
            .interface(MANAGER_INTERFACE)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|source| bus_error(operation, source))
    }

    async fn proxy(
        &self,
        path: OwnedObjectPath,
        interface: &'static str,
        operation: &'static str,
    ) -> Result<zbus::Proxy<'_>, Error> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(DESTINATION)
            .path(path)
            .interface(interface)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|source| bus_error(operation, source))
    }

    async fn get_unit(&self, name: &str) -> Result<OwnedObjectPath, zbus::Error> {
        let manager = zbus::Proxy::new(
            &self.connection,
            DESTINATION,
            MANAGER_PATH,
            MANAGER_INTERFACE,
        )
        .await?;
        manager.call("GetUnit", &(name,)).await
    }

    async fn reload(&self, operation: &'static str) -> Result<(), Error> {
        self.manager(operation)
            .await?
            .call::<_, _, ()>("Reload", &())
            .await
            .map_err(|source| bus_error(operation, source))
    }

    /// Reads state, process identity, and command facts of one unit.
    ///
    /// Unit state, main PID, and control group are read twice around the
    /// process checks; any change, a reused PID, or a main process outside the
    /// unit's control group is a [`Error::Race`]. A failed command read is
    /// also a race when the unit changed meanwhile. A starting unit whose main
    /// process does not run `ExecStart` yet is observed without a process.
    async fn observe(&self, name: &str, id: &ServiceId) -> Result<ServiceObservation, Error> {
        const OPERATION: &str = "inspect";

        let path = self
            .get_unit(name)
            .await
            .map_err(|source| unit_error(OPERATION, id, source))?;
        let unit = self.proxy(path.clone(), UNIT_INTERFACE, OPERATION).await?;
        let service = self.proxy(path, SERVICE_INTERFACE, OPERATION).await?;
        let before = read_snapshot(&unit, &service, "inspect_snapshot").await?;
        let commands: Vec<ExecCommand> = service
            .get_property("ExecStart")
            .await
            .map_err(|source| snapshot_error("inspect_exec_start", source))?;
        let definition = match parse_exec_start(commands) {
            Ok(definition) => definition,
            Err(error) => {
                // A unit being unloaded reports no `ExecStart`; only a unit
                // that is still loaded and unchanged is a broken definition.
                let load_state: String = unit
                    .get_property("LoadState")
                    .await
                    .map_err(|source| snapshot_error("inspect_load_state", source))?;
                let after = read_snapshot(&unit, &service, "reinspect_snapshot").await?;
                if load_state != "loaded" || before != after {
                    return Err(inspect_race());
                }
                return Err(error);
            }
        };
        let inspector = LinuxInspector::new();
        let process = if before.main_pid == 0 {
            None
        } else {
            let identity_before = inspector
                .identity(before.main_pid)
                .map_err(|source| process_error("inspect_process_identity", source))?
                .ok_or_else(inspect_race)?;
            let in_control_group = inspector
                .is_in_control_group(before.main_pid, &before.control_group)
                .map_err(|source| process_error("inspect_process_control_group", source))?;
            let live = match read_live_command(
                inspector,
                identity_before,
                before.active_state == "activating",
            ) {
                Ok(live) => live,
                Err(error) => {
                    // The snapshot's properties are read one call at a time,
                    // so a restart can pair a stale `ActiveState` with the
                    // next fork's `MainPID`; a changed unit makes it a race.
                    let after = read_snapshot(&unit, &service, "reinspect_snapshot").await?;
                    validate_stable_observation(&before, &after)?;
                    return Err(error);
                }
            };
            let after = read_snapshot(&unit, &service, "reinspect_snapshot").await?;
            let identity_after = inspector
                .identity(before.main_pid)
                .map_err(|source| process_error("reinspect_process_identity", source))?
                .ok_or_else(inspect_race)?;
            validate_stable_observation(&before, &after)?;
            validate_process_binding(identity_before, identity_after, in_control_group)?;
            let bound = match &live {
                Some(live) => verify_live_command(&definition, live, &before.active_state)?,
                None => false,
            };
            bound.then_some(identity_after)
        };
        let after = read_snapshot(&unit, &service, "reinspect_snapshot").await?;
        validate_stable_observation(&before, &after)?;
        Ok(ServiceObservation {
            id: id.clone(),
            state: portable_state(&after.active_state, &after.sub_state),
            process,
            definition: Some(definition),
        })
    }

    /// Reads the recorded end of one unit; `None` unless it is `failed`.
    async fn exit_evidence(
        &self,
        name: &str,
        id: &ServiceId,
    ) -> Result<Option<ExitEvidence>, Error> {
        const OPERATION: &str = "exit_evidence";

        let path = self
            .get_unit(name)
            .await
            .map_err(|source| unit_error(OPERATION, id, source))?;
        let unit = self.proxy(path.clone(), UNIT_INTERFACE, OPERATION).await?;
        let service = self.proxy(path, SERVICE_INTERFACE, OPERATION).await?;
        let before = read_invocation(&unit, id, true).await?;
        if before.active_state != "failed" {
            return Ok(None);
        }
        let result: String = service
            .get_property("Result")
            .await
            .map_err(|source| unit_error(OPERATION, id, source))?;
        let code: i32 = service
            .get_property("ExecMainCode")
            .await
            .map_err(|source| unit_error(OPERATION, id, source))?;
        let status: i32 = service
            .get_property("ExecMainStatus")
            .await
            .map_err(|source| unit_error(OPERATION, id, source))?;
        // Optional detail: a manager that lacks a property still yields the
        // verdict above.
        let oom_kills = optional_counter(&service, "OOMKills", id).await?;
        let managed_oom_kills = optional_counter(&service, "ManagedOOMKills", id).await?;
        let memory_peak = optional_counter(&service, "MemoryPeak", id).await?;
        let exited_at = optional_counter(&service, "ExecMainExitTimestamp", id).await?;
        let after = read_invocation(&unit, id, false).await?;
        if before != after {
            return Err(Error::Race {
                operation: OPERATION,
            });
        }
        Ok(Some(
            ExitEvidence::new(decode_result(result)?, decode_exit(code, status))
                .with_oom_kills(oom_kills, managed_oom_kills)
                .with_memory_peak(memory_peak)
                .with_exited_at(exited_at.and_then(decode_timestamp)),
        ))
    }

    /// Stops a unit and waits until its control group is proven empty.
    ///
    /// The wait is bounded by the unit's own `TimeoutStopUSec` for both stop
    /// phases plus [`SETTLE_MARGIN`]. A unit left `failed` is reset only after
    /// its control group is empty, so a transient unit disappears.
    async fn stop_and_settle(
        &self,
        name: &str,
        id: &ServiceId,
        operation: &'static str,
    ) -> Result<(), Error> {
        let path = self
            .get_unit(name)
            .await
            .map_err(|source| unit_error(operation, id, source))?;
        let service = self.proxy(path, SERVICE_INTERFACE, operation).await?;
        let stop_timeout: u64 = service
            .get_property("TimeoutStopUSec")
            .await
            .map_err(|source| unit_error(operation, id, source))?;
        let manager = self.manager(operation).await?;
        let _job: OwnedObjectPath = manager
            .call("StopUnit", &(name, STOP_MODE))
            .await
            .map_err(|source| unit_error(operation, id, source))?;
        let deadline = Instant::now() + settle_bound(stop_timeout);
        loop {
            match self.settle_state(name, operation).await? {
                Settled::Gone | Settled::Ended { failed: false } => return Ok(()),
                Settled::Ended { failed: true } => {
                    return match manager.call::<_, _, ()>("ResetFailedUnit", &(name,)).await {
                        Ok(()) => Ok(()),
                        Err(source) if classify(&source) == Fault::Missing => Ok(()),
                        Err(source) => Err(bus_error(operation, source)),
                    };
                }
                Settled::Busy => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout { operation });
            }
            async_io::Timer::after(SETTLE_POLL_INTERVAL).await;
        }
    }

    /// Stops one slice when it is loaded and its control group is empty.
    ///
    /// A missing slice or one that still holds processes is left alone.
    async fn stop_empty_slice(&self, name: &str, operation: &'static str) -> Result<(), Error> {
        let manager = self.manager(operation).await?;
        let processes: Vec<UnitProcess> = match manager.call("GetUnitProcesses", &(name,)).await {
            Ok(processes) => processes,
            Err(source) if classify(&source) == Fault::Missing => return Ok(()),
            Err(source) => return Err(bus_error(operation, source)),
        };
        if !processes.is_empty() {
            return Ok(());
        }
        match manager
            .call::<_, _, OwnedObjectPath>("StopUnit", &(name, STOP_MODE))
            .await
        {
            Ok(_job) => Ok(()),
            Err(source) if classify(&source) == Fault::Missing => Ok(()),
            Err(source) => Err(bus_error(operation, source)),
        }
    }

    /// Reads `ActiveState` and `SubState` of a loaded unit; `None` when the
    /// manager has not loaded it.
    async fn active_state(
        &self,
        name: &str,
        operation: &'static str,
    ) -> Result<Option<(String, String)>, Error> {
        let path = match self.get_unit(name).await {
            Ok(path) => path,
            Err(source) if classify(&source) == Fault::Missing => return Ok(None),
            Err(source) => return Err(bus_error(operation, source)),
        };
        let unit = self.proxy(path, UNIT_INTERFACE, operation).await?;
        let mut states = Vec::with_capacity(2);
        for property in ["ActiveState", "SubState"] {
            match unit.get_property::<String>(property).await {
                Ok(state) => states.push(state),
                Err(source) if classify(&source) == Fault::Missing => return Ok(None),
                Err(source) => return Err(bus_error(operation, source)),
            }
        }
        let sub = states.pop().expect("two states were read");
        let active = states.pop().expect("two states were read");
        Ok(Some((active, sub)))
    }

    /// Stops one timer and waits until it has ended.
    ///
    /// A timer runs no process, so ending is immediate; the wait only orders
    /// the stop before later calls. A timer the manager has not loaded is
    /// already stopped, and one left `failed` is reset.
    async fn stop_timer(&self, name: &str, operation: &'static str) -> Result<(), Error> {
        if self.active_state(name, operation).await?.is_none() {
            return Ok(());
        }
        let manager = self.manager(operation).await?;
        match manager
            .call::<_, _, OwnedObjectPath>("StopUnit", &(name, STOP_MODE))
            .await
        {
            Ok(_job) => {}
            Err(source) if classify(&source) == Fault::Missing => return Ok(()),
            Err(source) => return Err(bus_error(operation, source)),
        }
        let deadline = Instant::now() + SETTLE_MARGIN;
        loop {
            match self.active_state(name, operation).await? {
                None => return Ok(()),
                Some((active, _)) if is_ended(&active) => {
                    if active == "failed" {
                        match manager.call::<_, _, ()>("ResetFailedUnit", &(name,)).await {
                            Ok(()) => {}
                            Err(source) if classify(&source) == Fault::Missing => {}
                            Err(source) => return Err(bus_error(operation, source)),
                        }
                    }
                    return Ok(());
                }
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout { operation });
            }
            async_io::Timer::after(SETTLE_POLL_INTERVAL).await;
        }
    }

    async fn settle_state(&self, name: &str, operation: &'static str) -> Result<Settled, Error> {
        let path = match self.get_unit(name).await {
            Ok(path) => path,
            Err(source) if classify(&source) == Fault::Missing => return Ok(Settled::Gone),
            Err(source) => return Err(bus_error(operation, source)),
        };
        let unit = self.proxy(path, UNIT_INTERFACE, operation).await?;
        let state = match unit.get_property::<String>("ActiveState").await {
            Ok(state) => state,
            Err(source) if classify(&source) == Fault::Missing => return Ok(Settled::Gone),
            Err(source) => return Err(bus_error(operation, source)),
        };
        if !is_ended(&state) {
            return Ok(Settled::Busy);
        }
        let processes: Vec<UnitProcess> = match self
            .manager(operation)
            .await?
            .call("GetUnitProcesses", &(name,))
            .await
        {
            Ok(processes) => processes,
            Err(source) if classify(&source) == Fault::Missing => return Ok(Settled::Gone),
            Err(source) => return Err(bus_error(operation, source)),
        };
        Ok(if processes.is_empty() {
            Settled::Ended {
                failed: state == "failed",
            }
        } else {
            Settled::Busy
        })
    }
}

/// Progress of a unit after a stop request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// The manager no longer knows the unit.
    Gone,
    /// The unit is inactive or failed and its control group is empty.
    Ended { failed: bool },
    /// The unit is still stopping or still has processes.
    Busy,
}

fn is_ended(active_state: &str) -> bool {
    matches!(active_state, "inactive" | "failed")
}

fn settle_bound(stop_timeout_usec: u64) -> Duration {
    // `TimeoutStopUSec` may be `u64::MAX` (infinity) on a foreign definition;
    // the definition contract caps ours at `MAX_JOB_TIMEOUT`.
    Duration::from_micros(stop_timeout_usec).min(MAX_JOB_TIMEOUT) * STOP_PHASES + SETTLE_MARGIN
}

/// Builds the `StartTransientUnit` properties of one worker generation.
fn worker_properties(
    namespace: &Namespace,
    key: &WorkerKey,
    definition: &JobDefinition,
) -> Result<Vec<(&'static str, Variant<'static>)>, Error> {
    if definition.restart() != RestartPolicy::Never {
        return Err(Error::InvalidDefinition {
            detail: "a worker generation must never restart".to_owned(),
        });
    }
    let executable = definition
        .executable()
        .to_str()
        .ok_or_else(|| Error::InvalidDefinition {
            detail: "executable must be valid UTF-8".to_owned(),
        })?
        .to_owned();
    let working_directory = definition
        .working_directory()
        .to_str()
        .ok_or_else(|| Error::InvalidDefinition {
            detail: "working directory must be valid UTF-8".to_owned(),
        })?
        .to_owned();
    let mut argv = Vec::with_capacity(definition.arguments().len() + 1);
    argv.push(executable.clone());
    argv.extend(definition.arguments().iter().cloned());
    let environment: Vec<String> = definition
        .environment()
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    Ok(vec![
        (
            "Description",
            Variant::from(format!(
                "Pohunek session worker {} generation {}",
                key.session_id(),
                key.generation()
            )),
        ),
        (
            "ExecStartEx",
            Variant::from(vec![(executable, argv, vec![NO_ENV_EXPAND.to_owned()])]),
        ),
        ("Type", Variant::from("notify")),
        ("NotifyAccess", Variant::from("main")),
        ("KillMode", Variant::from("control-group")),
        ("SendSIGHUP", Variant::from(true)),
        ("Slice", Variant::from(namespace.sessions_slice())),
        ("Restart", Variant::from("no")),
        (
            "TimeoutStartUSec",
            Variant::from(microseconds(definition.start_timeout())?),
        ),
        (
            "TimeoutStopUSec",
            Variant::from(microseconds(definition.exit_timeout())?),
        ),
        ("Environment", Variant::from(environment)),
        ("WorkingDirectory", Variant::from(working_directory)),
        ("LimitNOFILE", Variant::from(definition.open_files())),
        ("LimitNOFILESoft", Variant::from(definition.open_files())),
        ("StandardOutput", Variant::from("journal")),
        ("StandardError", Variant::from("journal")),
    ])
}

/// Converts a validated timeout to systemd microseconds, rounding up so a
/// non-zero timeout never becomes `0` ("no timeout").
fn microseconds(value: Duration) -> Result<u64, Error> {
    u64::try_from(value.as_nanos().div_ceil(1_000)).map_err(|_overflow| Error::InvalidDefinition {
        detail: "timeout exceeds the systemd time range".to_owned(),
    })
}

fn parse_exec_start(commands: Vec<ExecCommand>) -> Result<DefinitionFacts, Error> {
    let count = commands.len();
    let Ok([(path, argv, ..)]) = <[ExecCommand; 1]>::try_from(commands) else {
        return Err(invalid_data(format!(
            "unit has {count} ExecStart commands; exactly one is required"
        )));
    };
    let Some((argv0, arguments)) = argv.split_first() else {
        return Err(invalid_data(
            "ExecStart has an empty argument vector".to_owned(),
        ));
    };
    if *argv0 != path || !Path::new(&path).is_absolute() {
        return Err(invalid_data(
            "ExecStart must name an absolute executable as argv[0]".to_owned(),
        ));
    }
    Ok(DefinitionFacts {
        executable: PathBuf::from(path),
        arguments: arguments.to_vec(),
    })
}

/// Executable and command line of a live main process.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveCommand {
    executable: PathBuf,
    cmdline: Vec<String>,
}

/// Reads the executable and command line of a unit's main process.
///
/// While the unit is `activating`, the main process may still be the manager's
/// fork before `execve`. It inherits the user manager's non-dumpable flag, so
/// procfs denies `/proc/<pid>/exe`; that denial means no command is running
/// yet (`Ok(None)`), and it is a real failure in any other state.
fn read_live_command(
    inspector: LinuxInspector,
    identity: ProcessIdentity,
    activating: bool,
) -> Result<Option<LiveCommand>, Error> {
    let executable = match inspector.executable(identity.pid) {
        Ok(Some(executable)) => executable,
        Ok(None) => return Err(inspect_race()),
        Err(source) if before_exec(&source, activating) => return Ok(None),
        Err(source) => return Err(process_error("inspect_process_executable", source)),
    };
    let fact = match inspector.process(identity.pid) {
        Ok(Some(fact)) => fact,
        Ok(None) => return Err(inspect_race()),
        Err(source) if before_exec(&source, activating) => return Ok(None),
        Err(source) => return Err(process_error("inspect_process_command", source)),
    };
    if fact.identity() != identity {
        return Err(inspect_race());
    }
    Ok(Some(LiveCommand {
        executable,
        cmdline: fact.cmdline,
    }))
}

/// Whether a process read failed because an activating unit has not run
/// `execve` yet.
fn before_exec(source: &ProcessError, activating: bool) -> bool {
    activating && matches!(source, ProcessError::PermissionDenied { .. })
}

/// The path of a live process image, without the ` (deleted)` the kernel
/// appends to `/proc/<pid>/exe` once the file was unlinked, unless a file of
/// that literal name exists.
///
/// A directory that was removed and published again leaves its running
/// workers on an unlinked image; the main process and its command line still
/// bind them to the unit.
fn live_image_path(executable: &Path) -> std::borrow::Cow<'_, Path> {
    use std::os::unix::ffi::OsStrExt as _;

    const SUFFIX: &[u8] = b" (deleted)";
    match executable.as_os_str().as_bytes().strip_suffix(SUFFIX) {
        Some(stripped) if !executable.exists() => {
            std::borrow::Cow::Owned(PathBuf::from(std::ffi::OsStr::from_bytes(stripped)))
        }
        _ => std::borrow::Cow::Borrowed(executable),
    }
}

/// Checks that the main process runs the command systemd recorded.
///
/// Returns whether the process is bound to the unit. The executable is
/// compared after resolving symlinks in the recorded path, because
/// `/proc/<pid>/exe` is always canonical. A worker whose version directory a
/// repair moved aside still reports the moved path, which counts as the
/// recorded executable ([`pohunek_paths::is_set_aside_copy`]). Empty arguments
/// are skipped on both sides since the process inspector omits them. While a unit is
/// `activating`, the main process may not run `ExecStart` yet, so a mismatch
/// then leaves it unbound rather than being invalid data.
fn verify_live_command(
    definition: &DefinitionFacts,
    live: &LiveCommand,
    active_state: &str,
) -> Result<bool, Error> {
    let image = live_image_path(&live.executable);
    let executable_matches = std::fs::canonicalize(&definition.executable).is_ok_and(|expected| {
        expected == *image || pohunek_paths::is_set_aside_copy(&expected, &image)
    });
    let expected: Vec<String> =
        std::iter::once(definition.executable.to_string_lossy().into_owned())
            .chain(definition.arguments.iter().cloned())
            .filter(|argument| !argument.is_empty())
            .collect();
    let cmdline_matches = live.cmdline == expected;
    if executable_matches && cmdline_matches {
        Ok(true)
    } else if active_state == "activating" {
        Ok(false)
    } else if executable_matches {
        Err(invalid_data(
            "main process command line differs from ExecStart".to_owned(),
        ))
    } else {
        Err(invalid_data(
            "main process executable differs from ExecStart".to_owned(),
        ))
    }
}

fn portable_state(active: &str, sub: &str) -> ServiceState {
    match active {
        "activating" => ServiceState::Starting,
        "active" | "reloading" => ServiceState::Running,
        "deactivating" => ServiceState::Stopping,
        "inactive" => ServiceState::Stopped,
        "failed" => ServiceState::Failed,
        _ if sub == "failed" => ServiceState::Failed,
        _ => ServiceState::Unknown,
    }
}

/// The identity of one run of a unit: its state and invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    active_state: String,
    invocation_id: Vec<u8>,
}

/// Reads the state and invocation of a unit, in the order that brackets the
/// reads between them: the invocation first when `opening`, last otherwise.
///
/// A restart between the two reads of one call changes the invocation seen by
/// the other call, so a state of one run is never paired with the properties
/// of another.
async fn read_invocation(
    unit: &zbus::Proxy<'_>,
    id: &ServiceId,
    opening: bool,
) -> Result<Invocation, Error> {
    const OPERATION: &str = "exit_evidence";

    async fn invocation_id(unit: &zbus::Proxy<'_>, id: &ServiceId) -> Result<Vec<u8>, Error> {
        unit.get_property("InvocationID")
            .await
            .map_err(|source| unit_error(OPERATION, id, source))
    }
    async fn active_state(unit: &zbus::Proxy<'_>, id: &ServiceId) -> Result<String, Error> {
        unit.get_property("ActiveState")
            .await
            .map_err(|source| unit_error(OPERATION, id, source))
    }
    if opening {
        let invocation_id = invocation_id(unit, id).await?;
        let active_state = active_state(unit, id).await?;
        Ok(Invocation {
            active_state,
            invocation_id,
        })
    } else {
        let active_state = active_state(unit, id).await?;
        let invocation_id = invocation_id(unit, id).await?;
        Ok(Invocation {
            active_state,
            invocation_id,
        })
    }
}

/// Reads one optional `t` property of the service interface.
async fn optional_counter(
    service: &zbus::Proxy<'_>,
    property: &'static str,
    id: &ServiceId,
) -> Result<Option<u64>, Error> {
    decode_counter(service.get_property::<u64>(property).await, id)
}

/// Decodes an optional counter: `UINT64_MAX` is the manager's "not available"
/// (the control group was never realized), and a manager that predates the
/// property reports `UnknownProperty`. Both are absent, never zero. Any other
/// failure keeps its mapping, so a vanished unit is still [`Error::NotFound`].
fn decode_counter(outcome: zbus::Result<u64>, id: &ServiceId) -> Result<Option<u64>, Error> {
    match outcome {
        Ok(u64::MAX) => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(source) if is_unknown_property(&source) => Ok(None),
        Err(source) => Err(unit_error("exit_evidence", id, source)),
    }
}

fn is_unknown_property(source: &zbus::Error) -> bool {
    match source {
        zbus::Error::MethodError(name, _, _) => name.as_str() == UNKNOWN_PROPERTY,
        zbus::Error::FDO(error) => {
            zbus::DBusError::name(error.as_ref()).as_str() == UNKNOWN_PROPERTY
        }
        _ => false,
    }
}

/// Decodes a realtime timestamp in microseconds since the Unix epoch; 0 means
/// the event never happened.
fn decode_timestamp(micros: u64) -> Option<SystemTime> {
    (micros != 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_micros(micros))
}

/// Accepts the manager's `Result` string when it is a short token.
fn decode_result(result: String) -> Result<String, Error> {
    let token = !result.is_empty()
        && result.len() <= MAX_RESULT_BYTES
        && result
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if token {
        Ok(result)
    } else {
        Err(Error::InvalidData {
            operation: "exit_evidence",
            detail: format!(
                "the unit result is not a short token ({} bytes)",
                result.len()
            ),
        })
    }
}

/// Decodes `ExecMainCode` and `ExecMainStatus` into how the main process
/// ended; `None` when no main process ended (code 0) or the code is unknown.
fn decode_exit(code: i32, status: i32) -> Option<JobExit> {
    match code {
        MAIN_EXITED => Some(JobExit::Code(status)),
        MAIN_KILLED | MAIN_DUMPED => Some(JobExit::Signal(status)),
        _ => None,
    }
}

fn validate_discovery_count(count: usize) -> Result<(), Error> {
    if count > MAX_DISCOVERED_WORKERS {
        return Err(Error::InvalidData {
            operation: "discover",
            detail: format!(
                "worker namespace contains {count} units; maximum is {MAX_DISCOVERED_WORKERS}"
            ),
        });
    }
    Ok(())
}

fn push_discovered_observation(
    discovery: &mut Discovery,
    result: Result<ServiceObservation, Error>,
) -> Result<(), Error> {
    match result {
        Ok(observation) => discovery.observations.push(observation),
        // The unit is gone; nothing can reference its version any more.
        Err(Error::NotFound(_)) => {}
        // The unit still exists but its identity is unproven, so a result
        // omitting it is incomplete; only the strict discovery refuses it.
        Err(Error::Race { .. }) => discovery.incomplete = true,
        Err(error) => return Err(error),
    }
    Ok(())
}

/// The reconciliation discovery: raced units are omitted, because discovery
/// is repeated anyway.
fn lenient_discovery(discovery: Discovery) -> Vec<ServiceObservation> {
    super::warn_rejected("systemd", &discovery.rejected_units);
    discovery.observations
}

/// The destructive discovery: a result that may omit a registered job fails,
/// so the caller keeps every version.
///
/// A raced unit is [`Error::Race`]. A rejected unit matches this namespace's
/// pattern and may still run, but its name never maps to a service ID, so it
/// is never observed and is [`Error::InvalidData`].
fn strict_discovery(discovery: Discovery) -> Result<Vec<ServiceObservation>, Error> {
    if discovery.incomplete {
        return Err(Error::Race {
            operation: "discover",
        });
    }
    super::warn_rejected("systemd", &discovery.rejected_units);
    if let Some(first) = discovery.rejected_units.first() {
        return Err(Error::InvalidData {
            operation: "discover",
            detail: format!(
                "{} worker unit(s) of this namespace cannot be verified, first `{first}`; \
                 such a unit may still run",
                discovery.rejected_units.len()
            ),
        });
    }
    Ok(discovery.observations)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeServiceSnapshot {
    active_state: String,
    sub_state: String,
    main_pid: u32,
    control_group: String,
}

async fn read_snapshot(
    unit: &zbus::Proxy<'_>,
    service: &zbus::Proxy<'_>,
    operation_name: &'static str,
) -> Result<NativeServiceSnapshot, Error> {
    Ok(NativeServiceSnapshot {
        active_state: unit
            .get_property("ActiveState")
            .await
            .map_err(|source| snapshot_error(operation_name, source))?,
        sub_state: unit
            .get_property("SubState")
            .await
            .map_err(|source| snapshot_error(operation_name, source))?,
        main_pid: service
            .get_property("MainPID")
            .await
            .map_err(|source| snapshot_error(operation_name, source))?,
        control_group: service
            .get_property("ControlGroup")
            .await
            .map_err(|source| snapshot_error(operation_name, source))?,
    })
}

fn validate_stable_observation(
    before: &NativeServiceSnapshot,
    after: &NativeServiceSnapshot,
) -> Result<(), Error> {
    if before != after {
        return Err(inspect_race());
    }
    Ok(())
}

fn validate_process_binding(
    before: ProcessIdentity,
    after: ProcessIdentity,
    in_control_group: bool,
) -> Result<(), Error> {
    if before != after || !in_control_group {
        return Err(inspect_race());
    }
    Ok(())
}

fn inspect_race() -> Error {
    Error::Race {
        operation: "inspect_service",
    }
}

fn invalid_data(detail: String) -> Error {
    Error::InvalidData {
        operation: "inspect",
        detail,
    }
}

fn process_error(operation_name: &'static str, source: ProcessError) -> Error {
    if source.is_race() {
        inspect_race()
    } else {
        Error::Operation {
            operation: operation_name,
            source: Box::new(source),
        }
    }
}

fn unavailable(
    operation: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> Error {
    Error::Unavailable {
        operation,
        source: Box::new(source),
    }
}

/// D-Bus failure classes this backend reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The unit or its object does not exist.
    Missing,
    /// A transient unit with this name already exists.
    Exists,
    /// The bus or the user manager cannot be reached.
    Unavailable,
    /// The call exceeded its deadline.
    TimedOut,
    /// Any other rejection.
    Other,
}

fn classify(source: &zbus::Error) -> Fault {
    match source {
        zbus::Error::MethodError(name, _, _) => classify_name(name.as_str()),
        zbus::Error::FDO(error) => classify_name(zbus::DBusError::name(error.as_ref()).as_str()),
        zbus::Error::InputOutput(error) if error.kind() == io::ErrorKind::TimedOut => {
            Fault::TimedOut
        }
        zbus::Error::InputOutput(_) | zbus::Error::Address(_) | zbus::Error::Handshake(_) => {
            Fault::Unavailable
        }
        _ => Fault::Other,
    }
}

fn classify_name(name: &str) -> Fault {
    match name {
        NO_SUCH_UNIT | UNKNOWN_OBJECT => Fault::Missing,
        UNIT_EXISTS => Fault::Exists,
        SERVICE_UNKNOWN | NAME_HAS_NO_OWNER | NO_SERVER | DISCONNECTED => Fault::Unavailable,
        NO_REPLY | TIMEOUT | TIMED_OUT => Fault::TimedOut,
        _ => Fault::Other,
    }
}

/// Maps a D-Bus failure that is not about one known unit.
fn bus_error(operation: &'static str, source: zbus::Error) -> Error {
    match classify(&source) {
        Fault::Unavailable => unavailable(operation, source),
        Fault::TimedOut => Error::Timeout { operation },
        Fault::Missing | Fault::Exists | Fault::Other => Error::Operation {
            operation,
            source: Box::new(source),
        },
    }
}

/// Maps a D-Bus failure of a call addressing the unit of `id`.
fn unit_error(operation: &'static str, id: &ServiceId, source: zbus::Error) -> Error {
    match classify(&source) {
        Fault::Missing => Error::NotFound(id.clone()),
        Fault::Exists => Error::AlreadyRegistered(id.clone()),
        Fault::Unavailable | Fault::TimedOut | Fault::Other => bus_error(operation, source),
    }
}

/// Maps a failure while reading a unit being observed; disappearance is a race.
fn snapshot_error(operation: &'static str, source: zbus::Error) -> Error {
    if classify(&source) == Fault::Missing {
        inspect_race()
    } else {
        bus_error(operation, source)
    }
}

fn fs_error(operation: &'static str, source: FsError) -> Error {
    Error::Operation {
        operation,
        source: Box::new(source),
    }
}

/// Opens the user unit directory, creating it only when `create` is set.
fn open_unit_dir(path: &Path, create: bool) -> Result<Option<TrustedDir>, Error> {
    match TrustedDir::open_absolute_owner_safe(path, UNIT_DIR_FORBIDDEN_BITS) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => {
            if create {
                TrustedDir::open_or_create_absolute(path, UNIT_DIR_MODE)
                    .map(Some)
                    .map_err(|source| fs_error("create_unit_directory", source))
            } else {
                Ok(None)
            }
        }
        Err(source) => Err(fs_error("open_unit_directory", source)),
    }
}

/// Atomically replaces one unit file with `contents`.
fn write_unit(directory: &TrustedDir, name: &str, contents: &str) -> Result<(), Error> {
    let mut entropy = [0_u8; TEMPORARY_NAME_ENTROPY];
    getrandom::getrandom(&mut entropy).map_err(|source| Error::Operation {
        operation: "write_unit_file",
        source: Box::new(io::Error::other(source.to_string())),
    })?;
    // A leading dot and a `.tmp` suffix keep systemd from loading the file
    // before it is complete.
    let mut temporary = format!(".{name}.");
    for byte in entropy {
        use std::fmt::Write as _;
        write!(temporary, "{byte:02x}").expect("writing hexadecimal to String cannot fail");
    }
    temporary.push_str(".tmp");
    directory
        .replace_file(name, &temporary, contents.as_bytes(), UNIT_FILE_MODE)
        .map_err(|source| Error::Operation {
            operation: "write_unit_file",
            source: Box::new(source),
        })
}

/// Removes one unit file; an absent file is not an error.
fn remove_unit(directory: &TrustedDir, name: &str) -> Result<(), Error> {
    const OPERATION: &str = "remove_unit_file";

    let Some(identity) = directory
        .entry_identity(name, EntryKind::RegularFile)
        .map_err(|source| fs_error(OPERATION, source))?
    else {
        return Ok(());
    };
    match directory
        .stage_random(name, REMOVAL_PREFIX, identity)
        .map_err(|source| fs_error(OPERATION, source))?
    {
        StageOutcome::Staged(entry) => {
            entry
                .remove()
                .map_err(|source| fs_error(OPERATION, source))?;
            Ok(())
        }
        StageOutcome::Missing => Ok(()),
        _ => Err(Error::Race {
            operation: OPERATION,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    /// Only an absent bus is "no bus": a socket that accepts and hangs up before
    /// the handshake is a bus that refuses the client, which stays an error.
    #[tokio::test]
    async fn an_absent_bus_is_no_bus_and_a_bus_that_hangs_up_is_not() {
        let dir = pohunek_test_support::tempdir().expect("scratch directory");
        let timeout = Duration::from_secs(5);

        let missing = dir.path().join("absent-bus");
        let error = read_user_manager_environment(
            Some(&format!("unix:path={}", missing.display())),
            timeout,
        )
        .await
        .expect_err("no bus listens at the socket");
        assert!(error.is_no_bus(), "{error}");
        assert!(!error.is_bus_address_invalid(), "{error}");
        // The error text does not carry the socket path: it names only the failure.
        assert!(
            !error.to_string().contains(&missing.display().to_string()),
            "{error}"
        );

        // A malformed address is a configuration error, not an absent bus, so it
        // fails closed even though no bus is reachable through it either.
        let error = read_user_manager_environment(Some("not an address"), timeout)
            .await
            .expect_err("the address cannot be parsed");
        assert!(!error.is_no_bus(), "{error}");
        assert!(!error.to_string().contains("not an address"), "{error}");

        // An address whose parse error quotes part of it is recognized, so a
        // caller reports the condition instead of the text.
        let error = read_user_manager_environment(Some("sentinel-token:"), timeout)
            .await
            .expect_err("the address cannot be parsed");
        assert!(error.is_bus_address_invalid(), "{error}");
        assert!(!error.is_no_bus(), "{error}");

        // A transport this build does not carry is an unusable address too: it
        // is never executed and never reads as an absent bus.
        let error = read_user_manager_environment(Some("unixexec:path=/bin/true"), timeout)
            .await
            .expect_err("the transport is not supported");
        assert!(error.is_bus_address_invalid(), "{error}");
        assert!(!error.is_no_bus(), "{error}");

        let socket = dir.path().join("hang-up-bus");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind the socket");
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        let error = read_user_manager_environment(
            Some(&format!("unix:path={}", socket.display())),
            timeout,
        )
        .await
        .expect_err("the bus hangs up before the handshake");
        assert!(!error.is_no_bus(), "{error}");
    }

    use crate::process::StartIdentity;
    use crate::supervisor::JobSpec;

    const SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN2";

    fn namespace() -> Namespace {
        Namespace::parse("0123456789ab").expect("valid namespace")
    }

    fn key() -> WorkerKey {
        WorkerKey::new(SESSION, "abcd2345").expect("valid worker key")
    }

    fn spec() -> JobSpec {
        JobSpec {
            executable: PathBuf::from("/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond"),
            arguments: vec![
                "--session-id".to_owned(),
                SESSION.to_owned(),
                "--worker-generation".to_owned(),
                "abcd2345".to_owned(),
            ],
            environment: BTreeMap::from([
                ("HOME".to_owned(), "/home/u".to_owned()),
                ("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".to_owned()),
            ]),
            working_directory: PathBuf::from("/home/u"),
            logs: None,
            start_timeout: Duration::from_secs(45),
            exit_timeout: Duration::from_secs(30),
            restart: RestartPolicy::Never,
            open_files: 8_192,
        }
    }

    #[test]
    fn worker_properties_follow_the_documented_table() {
        let definition = JobDefinition::new(spec()).expect("valid definition");
        let properties =
            worker_properties(&namespace(), &key(), &definition).expect("worker properties");
        let executable = "/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond".to_owned();
        let expected: Vec<(&str, Variant<'static>)> = vec![
            (
                "Description",
                Variant::from(format!(
                    "Pohunek session worker {SESSION} generation abcd2345"
                )),
            ),
            (
                "ExecStartEx",
                Variant::from(vec![(
                    executable.clone(),
                    vec![
                        executable,
                        "--session-id".to_owned(),
                        SESSION.to_owned(),
                        "--worker-generation".to_owned(),
                        "abcd2345".to_owned(),
                    ],
                    vec!["no-env-expand".to_owned()],
                )]),
            ),
            ("Type", Variant::from("notify")),
            ("NotifyAccess", Variant::from("main")),
            ("KillMode", Variant::from("control-group")),
            ("SendSIGHUP", Variant::from(true)),
            (
                "Slice",
                Variant::from("pohunek-0123456789ab-sessions.slice".to_owned()),
            ),
            ("Restart", Variant::from("no")),
            ("TimeoutStartUSec", Variant::from(45_000_000_u64)),
            ("TimeoutStopUSec", Variant::from(30_000_000_u64)),
            (
                "Environment",
                Variant::from(vec![
                    "HOME=/home/u".to_owned(),
                    "XDG_RUNTIME_DIR=/run/user/1000".to_owned(),
                ]),
            ),
            ("WorkingDirectory", Variant::from("/home/u".to_owned())),
            ("LimitNOFILE", Variant::from(8_192_u64)),
            ("LimitNOFILESoft", Variant::from(8_192_u64)),
            ("StandardOutput", Variant::from("journal")),
            ("StandardError", Variant::from("journal")),
        ];
        assert_eq!(properties, expected);
        assert_eq!(
            zbus::Signature::from(properties[1].1.value_signature()).to_string(),
            "a(sasas)"
        );
    }

    #[test]
    fn worker_properties_keep_hostile_values_verbatim() {
        let mut hostile = spec();
        hostile.arguments = vec![
            "a b".to_owned(),
            "$HOME".to_owned(),
            "%h".to_owned(),
            "quote\"".to_owned(),
            "back\\slash".to_owned(),
            "new\nline".to_owned(),
        ];
        let definition = JobDefinition::new(hostile.clone()).expect("valid definition");
        let properties =
            worker_properties(&namespace(), &key(), &definition).expect("worker properties");
        let Variant::Array(commands) = &properties[1].1 else {
            panic!("ExecStartEx is an array");
        };
        let rendered = format!("{commands:?}");
        assert!(rendered.contains("\"no-env-expand\""), "{rendered}");
        for argument in &hostile.arguments {
            assert!(
                rendered.contains(&format!("{argument:?}")),
                "{argument:?} missing from {rendered}"
            );
        }
    }

    #[test]
    fn restarting_worker_definitions_are_rejected() {
        let mut restarting = spec();
        restarting.restart = RestartPolicy::OnFailure {
            throttle: Duration::from_secs(1),
        };
        let definition = JobDefinition::new(restarting).expect("valid definition");
        assert!(matches!(
            worker_properties(&namespace(), &key(), &definition),
            Err(Error::InvalidDefinition { .. })
        ));
    }

    #[test]
    fn timeouts_round_up_to_whole_microseconds() {
        assert_eq!(microseconds(Duration::from_nanos(1)).expect("fits"), 1);
        assert_eq!(microseconds(Duration::from_millis(2)).expect("fits"), 2_000);
    }

    #[test]
    fn settle_bound_covers_both_stop_phases_and_caps_infinity() {
        assert_eq!(
            settle_bound(1_000_000),
            Duration::from_secs(2) + SETTLE_MARGIN
        );
        assert_eq!(
            settle_bound(u64::MAX),
            MAX_JOB_TIMEOUT * STOP_PHASES + SETTLE_MARGIN
        );
    }

    fn command(path: &str, argv: &[&str]) -> ExecCommand {
        (
            path.to_owned(),
            argv.iter().map(|&value| value.to_owned()).collect(),
            false,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        )
    }

    #[test]
    fn exec_start_facts_require_one_absolute_command() {
        assert_eq!(
            parse_exec_start(vec![command("/bin/w", &["/bin/w", "--a", "b"])])
                .expect("valid facts"),
            DefinitionFacts {
                executable: PathBuf::from("/bin/w"),
                arguments: vec!["--a".to_owned(), "b".to_owned()],
            }
        );
        for invalid in [
            Vec::new(),
            vec![
                command("/bin/w", &["/bin/w"]),
                command("/bin/x", &["/bin/x"]),
            ],
            vec![command("/bin/w", &[])],
            vec![command("/bin/w", &["w"])],
            vec![command("bin/w", &["bin/w"])],
        ] {
            assert!(matches!(
                parse_exec_start(invalid),
                Err(Error::InvalidData {
                    operation: "inspect",
                    ..
                })
            ));
        }
    }

    #[test]
    fn live_commands_must_match_exec_start() {
        let executable = std::fs::canonicalize("/proc/self/exe").expect("own executable");
        let facts = DefinitionFacts {
            executable: executable.clone(),
            arguments: vec!["--flag".to_owned(), String::new(), "value".to_owned()],
        };
        let path = executable.to_string_lossy().into_owned();
        let live = LiveCommand {
            executable: executable.clone(),
            cmdline: vec![path.clone(), "--flag".to_owned(), "value".to_owned()],
        };
        assert!(verify_live_command(&facts, &live, "active").expect("matching command"));
        assert!(verify_live_command(&facts, &live, "activating").expect("matching command"));

        let other_executable = LiveCommand {
            executable: PathBuf::from("/usr/lib/systemd/systemd-executor"),
            ..live.clone()
        };
        assert!(matches!(
            verify_live_command(&facts, &other_executable, "active"),
            Err(Error::InvalidData { .. })
        ));
        assert!(
            !verify_live_command(&facts, &other_executable, "activating")
                .expect("a starting unit may not run ExecStart yet"),
            "the pre-exec process is not bound"
        );
        let other_arguments = LiveCommand {
            cmdline: vec![path, "--other".to_owned()],
            ..live
        };
        assert!(matches!(
            verify_live_command(&facts, &other_arguments, "active"),
            Err(Error::InvalidData { .. })
        ));
        let missing = DefinitionFacts {
            executable: PathBuf::from("/nonexistent/pohunek-sessiond"),
            arguments: Vec::new(),
        };
        assert!(matches!(
            verify_live_command(&missing, &other_executable, "active"),
            Err(Error::InvalidData { .. })
        ));
    }

    /// A worker whose version directory a repair moved aside keeps running
    /// the image it started with: the recorded path names the republished
    /// directory and the process reports the moved one.
    #[test]
    fn a_live_command_in_a_set_aside_version_directory_matches_exec_start() {
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let versions = std::fs::canonicalize(root.path()).expect("canonical root");
        let image = versions.join("1.0.0/pohunek-sessiond");
        std::fs::create_dir_all(image.parent().expect("parent")).expect("version directory");
        std::fs::write(&image, b"").expect("republished image");
        let facts = DefinitionFacts {
            executable: image.clone(),
            arguments: Vec::new(),
        };
        let live = |executable: PathBuf| LiveCommand {
            executable,
            cmdline: vec![image.to_string_lossy().into_owned()],
        };

        let unlinked = PathBuf::from(format!("{} (deleted)", image.display()));
        assert!(
            verify_live_command(&facts, &live(unlinked), "active").expect("unlinked image"),
            "a republished directory leaves the worker on an unlinked image"
        );
        let aside = versions.join(".pohunek_refused-1.0.0-0123456789abcdef/pohunek-sessiond");
        assert!(verify_live_command(&facts, &live(aside), "active").expect("moved image"));
        for other in [
            ".pohunek_refused-2.0.0-0123456789abcdef/pohunek-sessiond",
            ".pohunek_refused-1.0.0-0123456789abcde/pohunek-sessiond",
            ".pohunek_refused-1.0.0-0123456789abcdef/other",
            "1.0.1/pohunek-sessiond",
            ".pohunek_refused-1.0.0-0123456789abcdef/nested/pohunek-sessiond",
        ] {
            assert!(
                matches!(
                    verify_live_command(&facts, &live(versions.join(other)), "active"),
                    Err(Error::InvalidData { .. })
                ),
                "{other}"
            );
        }
        // Only the daemon and worker images are accepted: another binary in
        // the same layout is a different unit's image.
        let plugin = versions.join("1.0.0/plugin-tool");
        std::fs::write(&plugin, b"").expect("plugin image");
        let plugin_facts = DefinitionFacts {
            executable: plugin.clone(),
            arguments: Vec::new(),
        };
        let moved_plugin = LiveCommand {
            executable: versions.join(".pohunek_refused-1.0.0-0123456789abcdef/plugin-tool"),
            cmdline: vec![plugin.to_string_lossy().into_owned()],
        };
        assert!(matches!(
            verify_live_command(&plugin_facts, &moved_plugin, "active"),
            Err(Error::InvalidData { .. })
        ));
    }

    #[test]
    fn denied_process_evidence_means_before_exec_only_while_activating() {
        let denied = || ProcessError::PermissionDenied {
            operation: "read_executable",
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(before_exec(&denied(), true));
        assert!(!before_exec(&denied(), false));
        assert!(!before_exec(
            &ProcessError::Race {
                operation: "read_executable"
            },
            true
        ));
        assert!(matches!(
            process_error("inspect_process_executable", denied()),
            Error::Operation {
                operation: "inspect_process_executable",
                ..
            }
        ));
    }

    #[test]
    fn unit_results_are_short_tokens_and_exit_codes_map_to_job_exits() {
        for accepted in ["exit-code", "oom-kill", "start-limit-hit", "core-dump"] {
            assert_eq!(
                decode_result(accepted.to_owned()).ok().as_deref(),
                Some(accepted)
            );
        }
        for rejected in [
            String::new(),
            "Exit-Code".to_owned(),
            "oom kill".to_owned(),
            "x".repeat(MAX_RESULT_BYTES + 1),
            "caf\u{e9}".to_owned(),
            "a\nb".to_owned(),
        ] {
            assert!(
                matches!(decode_result(rejected), Err(Error::InvalidData { .. })),
                "a hostile result is never copied"
            );
        }
        assert_eq!(decode_exit(MAIN_EXITED, 3), Some(JobExit::Code(3)));
        assert_eq!(decode_exit(MAIN_KILLED, 9), Some(JobExit::Signal(9)));
        assert_eq!(decode_exit(MAIN_DUMPED, 11), Some(JobExit::Signal(11)));
        assert_eq!(decode_exit(0, 0), None, "no main process ended");
        assert_eq!(decode_exit(42, 1), None, "an unknown code is not guessed");
    }

    #[test]
    fn optional_counters_are_absent_never_zero() {
        let id = key().service_id();
        assert_eq!(decode_counter(Ok(0), &id).ok(), Some(Some(0)));
        assert_eq!(decode_counter(Ok(4_096), &id).ok(), Some(Some(4_096)));
        assert_eq!(
            decode_counter(Ok(u64::MAX), &id).ok(),
            Some(None),
            "the manager's `not available` is absent"
        );
        assert_eq!(
            decode_counter(Err(method_error(UNKNOWN_PROPERTY)), &id).ok(),
            Some(None),
            "a manager without the property still yields the verdict"
        );
        assert!(
            matches!(
                decode_counter(Err(method_error(UNKNOWN_OBJECT)), &id),
                Err(Error::NotFound(_))
            ),
            "a vanished unit is not an absent property"
        );
        assert!(matches!(
            decode_counter(Err(method_error(NO_REPLY)), &id),
            Err(Error::Timeout { .. })
        ));
        assert_eq!(decode_timestamp(0), None);
        assert_eq!(
            decode_timestamp(1_500_000),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_500))
        );
    }

    #[test]
    fn native_states_map_to_portable_states() {
        assert_eq!(
            portable_state("activating", "start"),
            ServiceState::Starting
        );
        assert_eq!(portable_state("active", "running"), ServiceState::Running);
        assert_eq!(
            portable_state("deactivating", "stop-sigterm"),
            ServiceState::Stopping
        );
        assert_eq!(portable_state("inactive", "dead"), ServiceState::Stopped);
        assert_eq!(portable_state("failed", "failed"), ServiceState::Failed);
        assert_eq!(portable_state("maintenance", "dead"), ServiceState::Unknown);
    }

    #[test]
    fn discovery_rejects_an_unbounded_namespace() {
        validate_discovery_count(MAX_DISCOVERED_WORKERS).expect("bounded namespace");
        assert!(matches!(
            validate_discovery_count(MAX_DISCOVERED_WORKERS + 1),
            Err(Error::InvalidData {
                operation: "discover",
                ..
            })
        ));
    }

    fn observation(id: &str) -> ServiceObservation {
        ServiceObservation {
            id: ServiceId::parse(id).expect("valid service id"),
            state: ServiceState::Running,
            process: None,
            definition: None,
        }
    }

    #[test]
    fn discovery_skips_only_vanished_units_and_the_strict_one_refuses_a_race() {
        let remaining = observation("s-43.abcd2345");
        let mut discovery = Discovery::default();

        push_discovered_observation(
            &mut discovery,
            Err(Error::NotFound(
                ServiceId::parse("s-42.abcd2345").expect("valid service id"),
            )),
        )
        .expect("a vanished unit is an expected discovery race");
        push_discovered_observation(
            &mut discovery,
            Err(Error::Race {
                operation: "inspect_service",
            }),
        )
        .expect("a raced unit marks the discovery incomplete");
        push_discovered_observation(&mut discovery, Ok(remaining.clone()))
            .expect("remaining unit is retained");
        assert!(push_discovered_observation(
            &mut discovery,
            Err(invalid_data("mismatch".to_owned()))
        )
        .is_err());

        assert_eq!(discovery.observations, vec![remaining.clone()]);
        assert!(
            discovery.incomplete,
            "the raced unit may still be registered"
        );
        assert!(matches!(
            strict_discovery(discovery.clone()),
            Err(Error::Race {
                operation: "discover"
            })
        ));
        // The lenient caller still receives the units that were observed.
        assert_eq!(lenient_discovery(discovery), vec![remaining]);
    }

    #[test]
    fn a_complete_discovery_is_accepted_by_both_callers() {
        let remaining = observation("s-43.abcd2345");
        let mut discovery = Discovery::default();
        push_discovered_observation(&mut discovery, Ok(remaining.clone()))
            .expect("remaining unit is retained");

        assert!(!discovery.incomplete);
        assert_eq!(
            strict_discovery(discovery.clone()).expect("complete discovery"),
            vec![remaining.clone()]
        );
        assert_eq!(lenient_discovery(discovery), vec![remaining]);
    }

    #[test]
    fn only_the_strict_discovery_refuses_a_rejected_unit() {
        let remaining = observation("s-43.abcd2345");
        let rejected = "pohunek-0123456789ab-worker-bad.service".to_owned();
        let discovery = Discovery {
            observations: vec![remaining.clone()],
            rejected_units: vec![rejected.clone()],
            incomplete: false,
        };

        let error = strict_discovery(discovery.clone())
            .expect_err("a rejected unit of this namespace may still run");
        assert!(
            matches!(
                &error,
                Error::InvalidData { operation: "discover", detail }
                    if detail.starts_with("1 worker unit(s)") && detail.contains(&rejected)
            ),
            "{error}"
        );
        assert_eq!(lenient_discovery(discovery), vec![remaining]);
    }

    #[test]
    fn inspection_rejects_a_service_generation_change() {
        let before = NativeServiceSnapshot {
            active_state: "active".to_owned(),
            sub_state: "running".to_owned(),
            main_pid: 42,
            control_group: "/worker.slice/session.service".to_owned(),
        };
        let mut changed_pid = before.clone();
        changed_pid.main_pid = 43;
        validate_stable_observation(&before, &changed_pid).expect_err("changed main PID is a race");
        let mut changed_state = before.clone();
        changed_state.active_state = "deactivating".to_owned();
        changed_state.sub_state = "stop-sigterm".to_owned();
        validate_stable_observation(&before, &changed_state).expect_err("changed state is a race");
        let mut changed_control_group = before.clone();
        changed_control_group.control_group = "/worker.slice/replacement.service".to_owned();
        validate_stable_observation(&before, &changed_control_group)
            .expect_err("changed control group is a race");
        validate_stable_observation(&before, &before).expect("stable observation");
    }

    #[test]
    fn inspection_rejects_process_reuse_and_wrong_unit_membership() {
        let before = ProcessIdentity {
            pid: 42,
            start_identity: StartIdentity::new(100),
        };
        let reused = ProcessIdentity {
            pid: 42,
            start_identity: StartIdentity::new(101),
        };

        validate_process_binding(before, reused, true).expect_err("PID reuse is a race");
        validate_process_binding(before, before, false).expect_err("different cgroup is a race");
        validate_process_binding(before, before, true).expect("stable process binding");
    }

    fn method_error(name: &'static str) -> zbus::Error {
        let name = zbus::names::OwnedErrorName::try_from(name).expect("valid D-Bus error name");
        let message = zbus::Message::method_call("/org/example/Test", "Inspect")
            .build(&())
            .expect("build method call");
        zbus::Error::MethodError(name, None, message)
    }

    #[test]
    fn error_names_map_to_typed_errors() {
        let id = key().service_id();
        assert!(matches!(
            unit_error("start", &id, method_error(UNIT_EXISTS)),
            Error::AlreadyRegistered(found) if found == id
        ));
        for name in [NO_SUCH_UNIT, UNKNOWN_OBJECT] {
            assert!(matches!(
                unit_error("retire", &id, method_error(name)),
                Error::NotFound(found) if found == id
            ));
            assert!(matches!(
                snapshot_error("snapshot", method_error(name)),
                Error::Race { .. }
            ));
            assert!(matches!(
                bus_error("discover", method_error(name)),
                Error::Operation { .. }
            ));
        }
        for name in [SERVICE_UNKNOWN, NAME_HAS_NO_OWNER, NO_SERVER, DISCONNECTED] {
            assert!(matches!(
                unit_error("start", &id, method_error(name)),
                Error::Unavailable {
                    operation: "start",
                    ..
                }
            ));
        }
        for name in [NO_REPLY, TIMEOUT, TIMED_OUT] {
            assert!(matches!(
                unit_error("start", &id, method_error(name)),
                Error::Timeout { operation: "start" }
            ));
        }
        for name in [
            "org.freedesktop.DBus.Error.InvalidArgs",
            "org.freedesktop.DBus.Error.UnknownProperty",
            "org.freedesktop.systemd1.TransactionIsDestructive",
        ] {
            assert!(matches!(
                unit_error("start", &id, method_error(name)),
                Error::Operation { .. }
            ));
        }
    }

    #[test]
    fn fdo_and_transport_errors_map_to_typed_errors() {
        let id = key().service_id();
        assert!(matches!(
            snapshot_error(
                "snapshot",
                zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject("gone".to_owned())))
            ),
            Error::Race { .. }
        ));
        assert!(matches!(
            unit_error(
                "inspect",
                &id,
                zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied("no".to_owned())))
            ),
            Error::Operation { .. }
        ));
        assert!(matches!(
            bus_error(
                "inspect",
                zbus::Error::InputOutput(std::sync::Arc::new(io::Error::from(
                    io::ErrorKind::TimedOut
                )))
            ),
            Error::Timeout { .. }
        ));
        assert!(matches!(
            bus_error(
                "connect",
                zbus::Error::InputOutput(std::sync::Arc::new(io::Error::from(
                    io::ErrorKind::ConnectionRefused
                )))
            ),
            Error::Unavailable { .. }
        ));
    }

    /// The connection path must not need a Tokio runtime: the backend is also
    /// driven by callers that run it on a plain thread.
    #[test]
    fn connect_without_a_tokio_runtime_reports_an_unreachable_bus() {
        /// Upper bound for the refused connect; a missing socket fails at
        /// once, so the bound only matters if the connect path hangs.
        const CONNECT_BOUND: Duration = Duration::from_secs(5);
        if !pohunek_test_support::process_env::in_own_process() {
            return;
        }
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let missing_bus = root.path().join("no-such-bus");
        let mut env = pohunek_test_support::process_env::ProcessEnv::lock();
        env.set(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", missing_bus.display()),
        );
        let outcome = std::thread::spawn(|| async_io::block_on(Bus::connect(CONNECT_BOUND)))
            .join()
            .expect("connecting outside a Tokio runtime must not panic");
        assert!(matches!(outcome, Err(Error::Unavailable { .. })));
    }

    /// Reading the user manager's environment reports an unreachable session
    /// bus instead of an empty environment.
    #[test]
    fn the_user_manager_environment_is_unavailable_without_a_session_bus() {
        const CALL_BOUND: Duration = Duration::from_secs(5);
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let missing_bus = root.path().join("no-such-bus");
        let address = format!("unix:path={}", missing_bus.display());
        let outcome = std::thread::spawn(move || {
            async_io::block_on(read_user_manager_environment(Some(&address), CALL_BOUND))
        })
        .join()
        .expect("reading outside a Tokio runtime must not panic");
        assert!(
            matches!(outcome, Err(Error::Unavailable { .. })),
            "{outcome:?}"
        );
    }
}
