//! Native systemd user-manager client for worker units.

use pohunek_platform::process::{LinuxInspector, ProcessInspector};
use pohunek_platform::supervisor::{
    Error as SupervisorError, ServiceId, ServiceObservation, ServiceState,
};
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

// Rust guideline compliant 2026-09-14

const SYSTEMD_DESTINATION: &str = "org.freedesktop.systemd1";
const SYSTEMD_MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const SYSTEMD_UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const SYSTEMD_SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";
const START_MODE: &str = "replace";
const RESTART_MODE: &str = "fail";
const STOP_MODE: &str = "fail";
/// Bounds memory and D-Bus follow-up work if the manager returns an unexpectedly
/// large namespace.
const MAX_DISCOVERED_WORKERS: usize = 4_096;
/// Installed production template for durable session workers.
pub const DEFAULT_WORKER_UNIT_TEMPLATE: &str = "pohunek-session@.service";

/// Validated systemd template used to address durable worker instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitTemplate(String);

impl UnitTemplate {
    /// Parses a deliberately restricted systemd service template.
    ///
    /// # Errors
    ///
    /// Returns [`UnitsError::InvalidTemplate`] unless the value is an ASCII
    /// service template with exactly one instance marker.
    pub fn parse(value: impl Into<String>) -> Result<Self, UnitsError> {
        let value = value.into();
        let Some(prefix) = value.strip_suffix("@.service") else {
            return Err(UnitsError::InvalidTemplate(value));
        };
        if prefix.is_empty()
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(UnitsError::InvalidTemplate(value));
        }
        Ok(Self(value))
    }

    fn instance(&self, session_id: &str) -> String {
        self.0
            .replace("@.service", &format!("@{session_id}.service"))
    }
}

impl Default for UnitTemplate {
    fn default() -> Self {
        Self(DEFAULT_WORKER_UNIT_TEMPLATE.to_owned())
    }
}

/// Errors returned by native user-manager operations.
#[derive(Debug, thiserror::Error)]
pub enum UnitsError {
    /// Session D-Bus or systemd rejected an operation.
    #[error("systemd user-manager operation failed: {0}")]
    Bus(#[from] zbus::Error),
    /// A session ID cannot safely name a worker unit.
    #[error("invalid managed session id for worker unit: {0}")]
    InvalidSession(String),
    /// A configured template could escape the intended systemd unit namespace.
    #[error("invalid worker unit template: {0}")]
    InvalidTemplate(String),
}

/// Cloneable client for the systemd user manager.
#[derive(Debug, Clone)]
pub struct Units {
    connection: zbus::Connection,
    template: UnitTemplate,
}

impl Units {
    /// Connects to the current user's D-Bus session.
    ///
    /// # Errors
    ///
    /// Returns [`UnitsError`] when the session bus is unavailable.
    pub async fn connect(template: UnitTemplate) -> Result<Self, UnitsError> {
        Ok(Self {
            connection: zbus::Connection::session().await?,
            template,
        })
    }

    /// Starts one worker unit without waiting for worker initialization.
    ///
    /// `StartUnit` returns after queuing the job. The caller must concurrently
    /// wait for the bootstrap socket; blocking on worker initialization here
    /// would deadlock a `Type=notify` worker.
    ///
    /// # Errors
    ///
    /// Returns [`UnitsError`] for an invalid ID or rejected D-Bus request.
    pub async fn start(&self, session_id: &str) -> Result<OwnedObjectPath, UnitsError> {
        let unit = unit_name(&self.template, session_id)?;
        let proxy = self.manager().await?;
        Ok(proxy.call("StartUnit", &(unit, START_MODE)).await?)
    }

    /// Replaces an old worker process for explicit native recovery.
    ///
    /// `RestartUnit` serializes the stop and start jobs in systemd. The caller
    /// must still reject the previous worker ID while waiting for the new
    /// bootstrap socket, because the old socket can remain connectable briefly.
    ///
    /// # Errors
    ///
    /// Returns [`UnitsError`] for an invalid ID or rejected D-Bus request.
    pub async fn restart(&self, session_id: &str) -> Result<OwnedObjectPath, UnitsError> {
        let unit = unit_name(&self.template, session_id)?;
        let proxy = self.manager().await?;
        Ok(proxy.call("RestartUnit", &(unit, RESTART_MODE)).await?)
    }

    /// Stops one worker unit for an explicit administrative cleanup.
    ///
    /// Normal `session.stop` uses the worker protocol and does not call this.
    ///
    /// # Errors
    ///
    /// Returns [`UnitsError`] for an invalid ID or rejected D-Bus request.
    pub async fn stop(&self, session_id: &str) -> Result<OwnedObjectPath, UnitsError> {
        let unit = unit_name(&self.template, session_id)?;
        let proxy = self.manager().await?;
        Ok(proxy.call("StopUnit", &(unit, STOP_MODE)).await?)
    }

    /// Discovers worker units inside the configured template namespace.
    ///
    /// # Errors
    ///
    /// Returns a typed supervisor error when D-Bus or process inspection fails.
    pub async fn discover(&self) -> Result<Vec<ServiceObservation>, SupervisorError> {
        type NativeUnit = (
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

        let manager = self
            .manager()
            .await
            .map_err(|source| unavailable("connect", source))?;
        let states: Vec<&str> = Vec::new();
        let pattern = self.template.pattern();
        let patterns = vec![pattern.as_str()];
        let units: Vec<NativeUnit> = manager
            .call("ListUnitsByPatterns", &(states, patterns))
            .await
            .map_err(|source| operation("discover", source))?;
        validate_discovery_count(units.len())?;
        let mut observations = Vec::with_capacity(units.len());
        for (name, _, _, _, _, _, _, _, _, _) in units {
            let Some(id) = self.template.service_id(&name) else {
                continue;
            };
            push_discovered_observation(&mut observations, self.inspect_service(&id).await)?;
        }
        observations.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(observations)
    }

    /// Reads portable state and process identity for one worker unit.
    ///
    /// # Errors
    ///
    /// Returns a typed supervisor error when the unit is absent or inspection
    /// fails.
    pub async fn inspect_service(
        &self,
        id: &ServiceId,
    ) -> Result<ServiceObservation, SupervisorError> {
        let session_id = id.as_str();
        let name = unit_name(&self.template, session_id)
            .map_err(|source| operation("validate_service_id", source))?;
        let manager = self
            .manager()
            .await
            .map_err(|source| unavailable("connect", source))?;
        let path: OwnedObjectPath = manager
            .call("GetUnit", &(name.as_str(),))
            .await
            .map_err(|source| map_get_unit_error(id, source))?;
        let unit = zbus::proxy::Builder::new(&self.connection)
            .destination(SYSTEMD_DESTINATION)
            .and_then(|builder| builder.path(path.clone()))
            .and_then(|builder| builder.interface(SYSTEMD_UNIT_INTERFACE))
            .map_err(|source| operation("inspect_unit", source))?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|source| operation("inspect_unit", source))?;
        let service = zbus::proxy::Builder::new(&self.connection)
            .destination(SYSTEMD_DESTINATION)
            .and_then(|builder| builder.path(path))
            .and_then(|builder| builder.interface(SYSTEMD_SERVICE_INTERFACE))
            .map_err(|source| operation("inspect_service", source))?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|source| operation("inspect_service", source))?;
        let before = read_service_snapshot(&unit, &service, "inspect_service_snapshot").await?;
        let inspector = LinuxInspector::new();
        let process = if before.main_pid == 0 {
            None
        } else {
            let identity_before = inspector
                .identity(before.main_pid)
                .map_err(|source| map_process_error("inspect_process_identity", source))?
                .ok_or_else(inspect_race)?;
            let in_control_group = inspector
                .is_in_control_group(before.main_pid, &before.control_group)
                .map_err(|source| map_process_error("inspect_process_control_group", source))?;
            let after =
                read_service_snapshot(&unit, &service, "reinspect_service_snapshot").await?;
            let identity_after = inspector
                .identity(before.main_pid)
                .map_err(|source| map_process_error("reinspect_process_identity", source))?
                .ok_or_else(inspect_race)?;
            validate_stable_observation(&before, &after)?;
            validate_process_binding(identity_before, identity_after, in_control_group)?;
            Some(identity_after)
        };
        let after = read_service_snapshot(&unit, &service, "reinspect_service_snapshot").await?;
        validate_stable_observation(&before, &after)?;
        Ok(ServiceObservation {
            id: id.clone(),
            state: portable_state(&after.active_state, &after.sub_state),
            process,
        })
    }

    async fn manager(&self) -> Result<zbus::Proxy<'_>, zbus::Error> {
        zbus::Proxy::new(
            &self.connection,
            SYSTEMD_DESTINATION,
            SYSTEMD_MANAGER_PATH,
            SYSTEMD_MANAGER_INTERFACE,
        )
        .await
    }
}

impl UnitTemplate {
    fn pattern(&self) -> String {
        self.0.replace("@.service", "@*.service")
    }

    fn service_id(&self, unit_name: &str) -> Option<ServiceId> {
        let (prefix, suffix) = self.0.split_once('@')?;
        let value = unit_name.strip_prefix(&format!("{prefix}@"))?;
        let value = value.strip_suffix(suffix)?;
        let id = ServiceId::parse(value).ok()?;
        pohunek_paths::valid_worker_session_id(id.as_str())
            .is_some()
            .then_some(id)
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

fn validate_discovery_count(count: usize) -> Result<(), SupervisorError> {
    if count > MAX_DISCOVERED_WORKERS {
        return Err(SupervisorError::InvalidData {
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
    result: Result<ServiceObservation, SupervisorError>,
) -> Result<(), SupervisorError> {
    match result {
        Ok(observation) => observations.push(observation),
        Err(SupervisorError::NotFound(_) | SupervisorError::Race { .. }) => {}
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

async fn read_service_snapshot(
    unit: &zbus::Proxy<'_>,
    service: &zbus::Proxy<'_>,
    operation_name: &'static str,
) -> Result<NativeServiceSnapshot, SupervisorError> {
    Ok(NativeServiceSnapshot {
        active_state: unit
            .get_property("ActiveState")
            .await
            .map_err(|source| map_snapshot_error(operation_name, source))?,
        sub_state: unit
            .get_property("SubState")
            .await
            .map_err(|source| map_snapshot_error(operation_name, source))?,
        main_pid: service
            .get_property("MainPID")
            .await
            .map_err(|source| map_snapshot_error(operation_name, source))?,
        control_group: service
            .get_property("ControlGroup")
            .await
            .map_err(|source| map_snapshot_error(operation_name, source))?,
    })
}

fn validate_stable_observation(
    before: &NativeServiceSnapshot,
    after: &NativeServiceSnapshot,
) -> Result<(), SupervisorError> {
    if before != after {
        return Err(inspect_race());
    }
    Ok(())
}

fn validate_process_binding(
    before: pohunek_platform::process::ProcessIdentity,
    after: pohunek_platform::process::ProcessIdentity,
    in_control_group: bool,
) -> Result<(), SupervisorError> {
    if before != after || !in_control_group {
        return Err(inspect_race());
    }
    Ok(())
}

fn inspect_race() -> SupervisorError {
    SupervisorError::Race {
        operation: "inspect_service",
    }
}

fn map_process_error(
    operation_name: &'static str,
    source: pohunek_platform::process::Error,
) -> SupervisorError {
    if source.is_race() {
        inspect_race()
    } else {
        operation(operation_name, source)
    }
}

fn unavailable(
    operation_name: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> SupervisorError {
    SupervisorError::Unavailable {
        operation: operation_name,
        source: Box::new(source),
    }
}

fn operation(
    operation_name: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> SupervisorError {
    SupervisorError::Operation {
        operation: operation_name,
        source: Box::new(source),
    }
}

fn map_get_unit_error(id: &ServiceId, source: zbus::Error) -> SupervisorError {
    if is_unit_disappearance(&source) {
        SupervisorError::NotFound(id.clone())
    } else {
        operation("inspect", source)
    }
}

fn map_snapshot_error(operation_name: &'static str, source: zbus::Error) -> SupervisorError {
    if is_unit_disappearance(&source) {
        inspect_race()
    } else {
        operation(operation_name, source)
    }
}

fn is_unit_disappearance(source: &zbus::Error) -> bool {
    match source {
        zbus::Error::MethodError(name, _, _) => is_unit_disappearance_name(name.as_str()),
        zbus::Error::FDO(error) => matches!(error.as_ref(), zbus::fdo::Error::UnknownObject(_)),
        _ => false,
    }
}

fn is_unit_disappearance_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.systemd1.NoSuchUnit" | "org.freedesktop.DBus.Error.UnknownObject"
    )
}

fn unit_name(template: &UnitTemplate, session_id: &str) -> Result<String, UnitsError> {
    pohunek_paths::valid_worker_session_id(session_id)
        .map(|id| template.instance(id))
        .ok_or_else(|| UnitsError::InvalidSession(session_id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_accept_only_managed_session_ids() {
        let template = UnitTemplate::default();
        assert_eq!(
            unit_name(&template, "s-42").expect("valid unit"),
            "pohunek-session@s-42.service"
        );
        assert_eq!(
            unit_name(&template, "s-01KYAPVPFVHD56Z69B9CX3XWN2").expect("valid ULID unit"),
            "pohunek-session@s-01KYAPVPFVHD56Z69B9CX3XWN2.service"
        );
        for invalid in [
            "",
            "s-",
            "s-a",
            "s-01kyapvpfvhd56z69b9cx3xwn2",
            "../s-1",
            "external-1",
        ] {
            assert!(matches!(
                unit_name(&template, invalid),
                Err(UnitsError::InvalidSession(value)) if value == invalid
            ));
        }
    }

    #[test]
    fn templates_allow_isolated_safe_namespaces_only() {
        let template =
            UnitTemplate::parse("pohunek-e2e-123_session@.service").expect("safe template");
        assert_eq!(
            unit_name(&template, "s-42").expect("valid unit"),
            "pohunek-e2e-123_session@s-42.service"
        );
        for invalid in [
            "",
            "pohunek-session.service",
            "@.service",
            "../pohunek-session@.service",
            "pohunek-session@.socket",
            "pohunek/session@.service",
        ] {
            assert!(matches!(
                UnitTemplate::parse(invalid),
                Err(UnitsError::InvalidTemplate(value)) if value == invalid
            ));
        }
    }

    #[test]
    fn templates_parse_only_their_own_service_ids() {
        let template = UnitTemplate::default();
        assert_eq!(
            template
                .service_id("pohunek-session@s-42.service")
                .expect("managed unit")
                .as_str(),
            "s-42"
        );
        assert!(template.service_id("other@s-42.service").is_none());
        assert!(template
            .service_id("pohunek-session@../x.service")
            .is_none());
    }

    #[test]
    fn native_states_map_to_portable_states() {
        assert_eq!(
            portable_state("activating", "start"),
            ServiceState::Starting
        );
        assert_eq!(portable_state("active", "running"), ServiceState::Running);
        assert_eq!(portable_state("inactive", "dead"), ServiceState::Stopped);
        assert_eq!(portable_state("failed", "failed"), ServiceState::Failed);
        assert_eq!(portable_state("maintenance", "dead"), ServiceState::Unknown);
    }

    #[test]
    fn discovery_rejects_an_unbounded_namespace() {
        validate_discovery_count(MAX_DISCOVERED_WORKERS).expect("bounded namespace");
        assert!(matches!(
            validate_discovery_count(MAX_DISCOVERED_WORKERS + 1),
            Err(SupervisorError::InvalidData {
                operation: "discover",
                ..
            })
        ));
    }

    #[test]
    fn discovery_skips_units_that_disappear_during_inspection() {
        let vanished = ServiceId::parse("s-42").expect("valid service id");
        let remaining = ServiceObservation {
            id: ServiceId::parse("s-43").expect("valid service id"),
            state: ServiceState::Running,
            process: None,
        };
        let mut observations = Vec::new();

        push_discovered_observation(&mut observations, Err(SupervisorError::NotFound(vanished)))
            .expect("vanished unit is an expected discovery race");
        push_discovered_observation(&mut observations, Ok(remaining.clone()))
            .expect("remaining unit is retained");

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
        let before = pohunek_platform::process::ProcessIdentity {
            pid: 42,
            start_identity: pohunek_platform::process::StartIdentity::new(100),
        };
        let reused = pohunek_platform::process::ProcessIdentity {
            pid: 42,
            start_identity: pohunek_platform::process::StartIdentity::new(101),
        };

        validate_process_binding(before, reused, true).expect_err("PID reuse is a race");
        validate_process_binding(before, before, false).expect_err("different cgroup is a race");
        validate_process_binding(before, before, true).expect("stable process binding");
    }

    #[test]
    fn discovery_skips_service_generation_races() {
        let mut observations = Vec::new();

        push_discovered_observation(
            &mut observations,
            Err(SupervisorError::Race {
                operation: "inspect_service",
            }),
        )
        .expect("generation changes are expected discovery races");

        assert!(observations.is_empty());
    }

    #[test]
    fn snapshot_errors_classify_only_unit_disappearance_as_a_race() {
        let no_such_unit = method_error("org.freedesktop.systemd1.NoSuchUnit");
        let unknown_object = method_error("org.freedesktop.DBus.Error.UnknownObject");
        let unknown_property = method_error("org.freedesktop.DBus.Error.UnknownProperty");
        let fdo_unknown_object =
            zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject("gone".to_owned())));
        let fdo_access_denied = zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied(
            "denied".to_owned(),
        )));

        for source in [no_such_unit, unknown_object, fdo_unknown_object] {
            assert!(matches!(
                map_snapshot_error("snapshot", source),
                SupervisorError::Race { .. }
            ));
        }
        for source in [unknown_property, fdo_access_denied] {
            assert!(matches!(
                map_snapshot_error("snapshot", source),
                SupervisorError::Operation { .. }
            ));
        }
    }

    fn method_error(name: &'static str) -> zbus::Error {
        let name = zbus::names::OwnedErrorName::try_from(name).expect("valid D-Bus error name");
        let message = zbus::Message::method_call("/org/example/Test", "Inspect")
            .expect("valid method call")
            .build(&())
            .expect("build method call");
        zbus::Error::MethodError(name, None, message)
    }
}
