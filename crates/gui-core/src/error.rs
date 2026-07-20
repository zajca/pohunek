//! Errors raised by the GUI core bridge.

use std::path::PathBuf;

use thiserror::Error;

use crate::{PromptError, ReviewStoreError};

/// Errors raised by the GUI core bridge.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Client(#[from] pohunek_client::ClientError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] protocol::ProtocolError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: String },
    #[error("remote assistant launch on `{host}` requires a project or repo target")]
    RemoteAssistantTargetRequired { host: String },
    #[error("degraded assistant launch is not supported for remote host `{host}`")]
    RemoteAssistantDegradedUnsupported { host: String },
    #[error("agent_state event is missing `{field}`")]
    MissingAgentStateField { field: &'static str },
    #[error("session event is missing `session`")]
    MissingSessionEventPayload,
    #[error("host discovery record does not contain a usable host name")]
    MissingDiscoveredHostName,
    #[error("provider `{provider}` context is missing a branch field")]
    MissingPromptBranch { provider: &'static str },
    #[error("provider link metadata is missing `{field}`")]
    MissingLinkField { field: &'static str },
    #[error("provider link metadata `{field}` is invalid")]
    InvalidLinkField { field: &'static str },
    #[error("project action resolved provider `{actual}` but provider item requires `{expected}`")]
    ProviderActionMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("provider launch item is inconsistent: {message}")]
    ProviderLaunchItemMismatch { message: &'static str },
    #[error("provider `{provider}` cannot be converted to a prompt provider")]
    UnsupportedPromptProvider { provider: &'static str },
    #[error(transparent)]
    ReviewStore(#[from] ReviewStoreError),
    #[error("review template `{}` is missing; run `pohunek setup` to install it", path.display())]
    MissingReviewTemplate { path: PathBuf },
    #[error("failed to read review template `{}`: {source}", path.display())]
    ReviewTemplateIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("session `{}` has no bound worktree to dispatch a review into", session_id.0)]
    ReviewSessionMissingWorktree { session_id: protocol::SessionId },
}
