//! Defines target-neutral durable service supervision.

use std::future::Future;
use std::pin::Pin;

use crate::process::ProcessIdentity;

// Rust guideline compliant 2026-09-14

const MAX_SERVICE_ID_BYTES: usize = 128;

/// Validated logical identifier in one supervisor-owned namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId(String);

impl ServiceId {
    /// Parses an identifier that cannot escape a backend namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] for empty, oversized, or unsafe
    /// identifiers.
    pub fn parse(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_SERVICE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::InvalidServiceId(value));
        }
        Ok(Self(value))
    }

    /// Returns the backend-independent identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Portable lifecycle state reported by a native supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Activation was accepted but the service is not ready yet.
    Starting,
    /// The service is active.
    Running,
    /// Deactivation is in progress.
    Stopping,
    /// The service is loaded but inactive.
    Stopped,
    /// The supervisor recorded a failed activation or process.
    Failed,
    /// The native backend returned a state without a portable equivalent.
    Unknown,
}

/// Native supervisor observation without backend-specific handles or names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceObservation {
    /// Logical identifier within the configured supervisor namespace.
    pub id: ServiceId,
    /// Portable lifecycle state.
    pub state: ServiceState,
    /// Stable process identity when the service currently has a main process.
    pub process: Option<ProcessIdentity>,
}

/// Errors shared by native supervisor backends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A logical identifier could escape the configured supervisor namespace.
    #[error("invalid supervisor service id: {0}")]
    InvalidServiceId(String),
    /// The requested logical service does not exist.
    #[error("supervisor service `{0}` was not found")]
    NotFound(ServiceId),
    /// The service changed generation during one observation.
    #[error("supervisor service changed during `{operation}`")]
    Race {
        /// Stable operation label.
        operation: &'static str,
    },
    /// The native supervisor or one of its required facilities is unavailable.
    #[error("supervisor operation `{operation}` is unavailable: {source}")]
    Unavailable {
        /// Stable operation label.
        operation: &'static str,
        /// Backend error retained for diagnostics.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The native supervisor rejected an otherwise valid operation.
    #[error("supervisor operation `{operation}` failed: {source}")]
    Operation {
        /// Stable operation label.
        operation: &'static str,
        /// Backend error retained for diagnostics.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A native response violated the shared supervisor contract.
    #[error("invalid supervisor response during `{operation}`: {detail}")]
    InvalidData {
        /// Stable operation label.
        operation: &'static str,
        /// Non-sensitive validation detail.
        detail: String,
    },
}

/// Boxed native supervisor operation.
pub type Operation<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Starts, discovers, inspects, replaces, and retires durable services.
pub trait Supervisor: std::fmt::Debug + Send + Sync {
    /// Starts a previously absent or inactive service.
    fn start<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()>;

    /// Discovers services in this backend's validated namespace.
    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>>;

    /// Inspects one service.
    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation>;

    /// Atomically replaces the service generation where the backend permits it.
    fn replace<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()>;

    /// Permanently retires the currently managed service generation.
    fn retire<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()>;
}

#[cfg(test)]
mod tests {
    use super::{Error, ServiceId};

    #[test]
    fn service_ids_are_bounded_and_namespace_safe() {
        for valid in ["s-42", "pohunek.worker_1", &"a".repeat(128)] {
            assert_eq!(
                ServiceId::parse(valid).expect("valid identifier").as_str(),
                valid
            );
        }
        for invalid in ["", "../worker", "worker/service", &"a".repeat(129)] {
            assert!(matches!(
                ServiceId::parse(invalid),
                Err(Error::InvalidServiceId(value)) if value == invalid
            ));
        }
    }
}
