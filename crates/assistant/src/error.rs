//! Errors raised by the assistant launch path.

use thiserror::Error;

/// Errors raised while connecting to a host or launching an assistant session.
#[derive(Debug, Error)]
pub enum AssistantError {
    /// The daemon connection or a request to it failed.
    #[error(transparent)]
    Client(#[from] pohunek_client::ClientError),
    /// A protocol-level error, including assistant-specific failures such as an
    /// unreadable knowledge bundle.
    #[error(transparent)]
    Protocol(#[from] protocol::ProtocolError),
    /// A required environment variable for path resolution is not set.
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: String },
    /// The application path configuration is invalid.
    #[error("invalid application path configuration: {source}")]
    Paths { source: pohunek_paths::PathError },
    /// A remote launch needs a project or repo target.
    #[error("remote assistant launch on `{host}` requires a project or repo target")]
    RemoteAssistantTargetRequired { host: String },
    /// Degraded launches cannot run against a remote host.
    #[error("degraded assistant launch is not supported for remote host `{host}`")]
    RemoteAssistantDegradedUnsupported { host: String },
}
