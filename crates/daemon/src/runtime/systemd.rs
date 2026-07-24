//! Native systemd user-manager client for worker units.

use zbus::zvariant::OwnedObjectPath;

// Rust guideline compliant 2026-07-23

const SYSTEMD_DESTINATION: &str = "org.freedesktop.systemd1";
const SYSTEMD_MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const SYSTEMD_SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";
const START_MODE: &str = "replace";
const RESTART_MODE: &str = "fail";
const STOP_MODE: &str = "fail";
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

/// Validated information about one systemd worker unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitInfo {
    /// Unit name used by the user manager.
    pub name: String,
    /// Current main process ID.
    pub main_pid: u32,
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

    /// Reads the main PID for one worker unit.
    ///
    /// # Errors
    ///
    /// Returns [`UnitsError`] when the unit is absent or D-Bus is unavailable.
    pub async fn inspect(&self, session_id: &str) -> Result<UnitInfo, UnitsError> {
        let name = unit_name(&self.template, session_id)?;
        let manager = self.manager().await?;
        let path: OwnedObjectPath = manager.call("GetUnit", &(name.as_str(),)).await?;
        let service = zbus::Proxy::new(
            &self.connection,
            SYSTEMD_DESTINATION,
            path,
            SYSTEMD_SERVICE_INTERFACE,
        )
        .await?;
        let main_pid = service.get_property("MainPID").await?;
        Ok(UnitInfo { name, main_pid })
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
        for invalid in ["", "s-", "s-a", "../s-1", "external-1"] {
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
}
