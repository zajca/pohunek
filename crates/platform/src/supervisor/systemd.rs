//! Supervises worker generations and the daemon through the systemd user manager.
//!
//! [`SystemdSupervisor`] starts every worker generation as a transient
//! service `pohunek-<ns>-worker-<session-id>-<generation>.service` through
//! `StartTransientUnit` on the session bus, so no unit file or template is
//! installed per worker. [`SystemdDaemon`] owns the only persistent units: the
//! daemon service and the sessions slice, written to the user unit directory
//! and enabled for login.
//!
//! # Supported systemd
//!
//! The minimum supported systemd is **255** (Ubuntu 24.04, the CI runner), the
//! oldest version this backend is exercised against. A probe on that runner
//! accepted `ExecStart`, `Type`, `NotifyAccess`, `KillMode`, `SendSIGHUP`,
//! `Slice`, `Restart`, both timeouts, and `Environment` as transient
//! properties. The real-manager tests (`tests/systemd.rs`) read every
//! property below back from the running manager; they pass on systemd 261.
//!
//! # Worker unit properties
//!
//! | Property | D-Bus type | Value | Reason |
//! |----------|-----------|-------|--------|
//! | `Description` | `s` | `Pohunek session worker <session-id> generation <generation>` | Identifies the unit in `systemctl` output. |
//! | `ExecStart` | `a(sasb)` | one command: the absolute executable, argv (`argv[0]` = executable), failure not ignored | Exact versioned argv; no shell. |
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

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, Value};

use super::{
    DaemonSupervisor, DefinitionFacts, Error, JobDefinition, Namespace, Operation, RestartPolicy,
    ServiceId, ServiceObservation, ServiceState, Supervisor, WorkerKey, MAX_JOB_TIMEOUT,
};
use crate::filesystem::{EntryKind, FsError, StageOutcome, TrustedDir};
use crate::process::{Error as ProcessError, LinuxInspector, ProcessIdentity, ProcessInspector};

mod unit_file;

#[doc(inline)]
pub use unit_file::{render_daemon_unit, render_sessions_slice};

// Rust guideline compliant 2026-09-24

const DESTINATION: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
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

const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";
const UNIT_EXISTS: &str = "org.freedesktop.systemd1.UnitExists";
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
    /// they are never inspected or retired.
    pub rejected_units: Vec<String>,
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
    /// Units that vanish or change generation while being inspected are
    /// skipped, because discovery is repeated by reconciliation anyway.
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
                    &mut discovery.observations,
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
            let auxiliary: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
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

    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move { Ok(self.discover_units().await?.observations) })
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

            let unit = render_daemon_unit(definition)?;
            let name = self.namespace.daemon_unit();
            if self.is_active(&name).await? {
                return Err(Error::AlreadyRegistered(self.id.clone()));
            }
            self.write_units(&unit, true)?;
            self.bus.reload(OPERATION).await?;
            let manager = self.bus.manager(OPERATION).await?;
            let _changes: (bool, Vec<(String, String, String)>) = manager
                .call("EnableUnitFiles", &(vec![name.as_str()], false, true))
                .await
                .map_err(|source| unit_error(OPERATION, &self.id, source))?;
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
            self.bus.reload(OPERATION).await
        })
    }
}

/// Session-bus connection to the systemd user manager.
#[derive(Debug, Clone)]
struct Bus {
    connection: zbus::Connection,
}

impl Bus {
    async fn connect(call_timeout: Duration) -> Result<Self, Error> {
        const OPERATION: &str = "connect";

        let connection = zbus::connection::Builder::session()
            .map_err(|source| unavailable(OPERATION, source))?
            .method_timeout(call_timeout)
            .build()
            .await
            .map_err(|source| match classify(&source) {
                Fault::TimedOut => Error::Timeout {
                    operation: OPERATION,
                },
                _ => unavailable(OPERATION, source),
            })?;
        Ok(Self { connection })
    }

    async fn manager(&self, operation: &'static str) -> Result<zbus::Proxy<'_>, Error> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(DESTINATION)
            .and_then(|builder| builder.path(MANAGER_PATH))
            .and_then(|builder| builder.interface(MANAGER_INTERFACE))
            .map_err(|source| bus_error(operation, source))?
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
            .and_then(|builder| builder.path(path))
            .and_then(|builder| builder.interface(interface))
            .map_err(|source| bus_error(operation, source))?
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
    /// unit's control group is a [`Error::Race`].
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
        let definition = parse_exec_start(commands)?;
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
            let live = read_live_command(
                inspector,
                identity_before,
                before.active_state == "activating",
            )?;
            let after = read_snapshot(&unit, &service, "reinspect_snapshot").await?;
            let identity_after = inspector
                .identity(before.main_pid)
                .map_err(|source| process_error("reinspect_process_identity", source))?
                .ok_or_else(inspect_race)?;
            validate_stable_observation(&before, &after)?;
            validate_process_binding(identity_before, identity_after, in_control_group)?;
            verify_live_command(&definition, &live, &before.active_state)?;
            Some(identity_after)
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
) -> Result<Vec<(&'static str, Value<'static>)>, Error> {
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
            Value::from(format!(
                "Pohunek session worker {} generation {}",
                key.session_id(),
                key.generation()
            )),
        ),
        ("ExecStart", Value::from(vec![(executable, argv, false)])),
        ("Type", Value::from("notify")),
        ("NotifyAccess", Value::from("main")),
        ("KillMode", Value::from("control-group")),
        ("SendSIGHUP", Value::from(true)),
        ("Slice", Value::from(namespace.sessions_slice())),
        ("Restart", Value::from("no")),
        (
            "TimeoutStartUSec",
            Value::from(microseconds(definition.start_timeout())?),
        ),
        (
            "TimeoutStopUSec",
            Value::from(microseconds(definition.exit_timeout())?),
        ),
        ("Environment", Value::from(environment)),
        ("WorkingDirectory", Value::from(working_directory)),
        ("LimitNOFILE", Value::from(definition.open_files())),
        ("LimitNOFILESoft", Value::from(definition.open_files())),
        ("StandardOutput", Value::from("journal")),
        ("StandardError", Value::from("journal")),
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
/// procfs denies `/proc/<pid>/exe`; that denial is a race then, and a real
/// failure in any other state.
fn read_live_command(
    inspector: LinuxInspector,
    identity: ProcessIdentity,
    activating: bool,
) -> Result<LiveCommand, Error> {
    let executable = inspector
        .executable(identity.pid)
        .map_err(|source| live_error("inspect_process_executable", source, activating))?
        .ok_or_else(inspect_race)?;
    let fact = inspector
        .process(identity.pid)
        .map_err(|source| live_error("inspect_process_command", source, activating))?
        .ok_or_else(inspect_race)?;
    if fact.identity() != identity {
        return Err(inspect_race());
    }
    Ok(LiveCommand {
        executable,
        cmdline: fact.cmdline,
    })
}

fn live_error(operation: &'static str, source: ProcessError, activating: bool) -> Error {
    if activating && matches!(source, ProcessError::PermissionDenied { .. }) {
        inspect_race()
    } else {
        process_error(operation, source)
    }
}

/// Checks that the main process runs the command systemd recorded.
///
/// The executable is compared after resolving symlinks in the recorded path,
/// because `/proc/<pid>/exe` is always canonical. Empty arguments are skipped
/// on both sides since the process inspector omits them. While a unit is
/// `activating`, the forked child may still be systemd's executor before it
/// runs `ExecStart`, so a mismatch then is a race rather than invalid data.
fn verify_live_command(
    definition: &DefinitionFacts,
    live: &LiveCommand,
    active_state: &str,
) -> Result<(), Error> {
    let executable_matches = std::fs::canonicalize(&definition.executable)
        .is_ok_and(|expected| expected == live.executable);
    let expected: Vec<String> =
        std::iter::once(definition.executable.to_string_lossy().into_owned())
            .chain(definition.arguments.iter().cloned())
            .filter(|argument| !argument.is_empty())
            .collect();
    let cmdline_matches = live.cmdline == expected;
    if executable_matches && cmdline_matches {
        Ok(())
    } else if active_state == "activating" {
        Err(inspect_race())
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
    observations: &mut Vec<ServiceObservation>,
    result: Result<ServiceObservation, Error>,
) -> Result<(), Error> {
    match result {
        Ok(observation) => observations.push(observation),
        Err(Error::NotFound(_) | Error::Race { .. }) => {}
        Err(error) => return Err(error),
    }
    Ok(())
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
        let expected: Vec<(&str, Value<'static>)> = vec![
            (
                "Description",
                Value::from(format!(
                    "Pohunek session worker {SESSION} generation abcd2345"
                )),
            ),
            (
                "ExecStart",
                Value::from(vec![(
                    executable.clone(),
                    vec![
                        executable,
                        "--session-id".to_owned(),
                        SESSION.to_owned(),
                        "--worker-generation".to_owned(),
                        "abcd2345".to_owned(),
                    ],
                    false,
                )]),
            ),
            ("Type", Value::from("notify")),
            ("NotifyAccess", Value::from("main")),
            ("KillMode", Value::from("control-group")),
            ("SendSIGHUP", Value::from(true)),
            (
                "Slice",
                Value::from("pohunek-0123456789ab-sessions.slice".to_owned()),
            ),
            ("Restart", Value::from("no")),
            ("TimeoutStartUSec", Value::from(45_000_000_u64)),
            ("TimeoutStopUSec", Value::from(30_000_000_u64)),
            (
                "Environment",
                Value::from(vec![
                    "HOME=/home/u".to_owned(),
                    "XDG_RUNTIME_DIR=/run/user/1000".to_owned(),
                ]),
            ),
            ("WorkingDirectory", Value::from("/home/u".to_owned())),
            ("LimitNOFILE", Value::from(8_192_u64)),
            ("LimitNOFILESoft", Value::from(8_192_u64)),
            ("StandardOutput", Value::from("journal")),
            ("StandardError", Value::from("journal")),
        ];
        assert_eq!(properties, expected);
        assert_eq!(
            zbus::zvariant::Signature::from(properties[1].1.value_signature()).to_string(),
            "a(sasb)"
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
        let Value::Array(commands) = &properties[1].1 else {
            panic!("ExecStart is an array");
        };
        let rendered = format!("{commands:?}");
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
        verify_live_command(&facts, &live, "active").expect("matching command");

        let other_executable = LiveCommand {
            executable: PathBuf::from("/usr/lib/systemd/systemd-executor"),
            ..live.clone()
        };
        assert!(matches!(
            verify_live_command(&facts, &other_executable, "active"),
            Err(Error::InvalidData { .. })
        ));
        assert!(matches!(
            verify_live_command(&facts, &other_executable, "activating"),
            Err(Error::Race { .. })
        ));
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

    #[test]
    fn denied_process_evidence_is_a_race_only_while_activating() {
        let denied = || ProcessError::PermissionDenied {
            operation: "read_executable",
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(matches!(
            live_error("inspect_process_executable", denied(), true),
            Error::Race { .. }
        ));
        assert!(matches!(
            live_error("inspect_process_executable", denied(), false),
            Error::Operation {
                operation: "inspect_process_executable",
                ..
            }
        ));
        assert!(matches!(
            live_error(
                "inspect_process_executable",
                ProcessError::Race {
                    operation: "read_executable"
                },
                false
            ),
            Error::Race { .. }
        ));
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
    fn discovery_skips_units_that_disappear_or_race_during_inspection() {
        let remaining = observation("s-43.abcd2345");
        let mut observations = Vec::new();

        push_discovered_observation(
            &mut observations,
            Err(Error::NotFound(
                ServiceId::parse("s-42.abcd2345").expect("valid service id"),
            )),
        )
        .expect("vanished unit is an expected discovery race");
        push_discovered_observation(
            &mut observations,
            Err(Error::Race {
                operation: "inspect_service",
            }),
        )
        .expect("generation changes are expected discovery races");
        push_discovered_observation(&mut observations, Ok(remaining.clone()))
            .expect("remaining unit is retained");
        assert!(push_discovered_observation(
            &mut observations,
            Err(invalid_data("mismatch".to_owned()))
        )
        .is_err());

        assert_eq!(observations, vec![remaining]);
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
            .expect("valid method call")
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
}
