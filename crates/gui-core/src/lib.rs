//! Headless state, SDK bridge logic, and command rendering for `pohunek-gui`.
//!
//! This crate intentionally has no Iced dependency. The native view layer wraps
//! these async helpers in Iced `Task` and `Subscription` values.

// Rust guideline compliant 2026-07-21
#![forbid(unsafe_code)]

pub mod assistant;
pub mod providers;

mod connection;
mod error;
mod link;
mod message;
mod review;
mod sdk;
mod state;
mod ui_state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_client::ClientOptions;
use protocol::{
    AgentKind, AgentRuntime, DaemonHealthResult, NotificationRecord, ProjectInfo, ProtocolVersion,
    SessionInfo,
};
use serde::{Deserialize, Serialize};

pub use pohunek_prompt::{
    render as render_prompt, Error as PromptError, Provider as PromptProvider,
};

#[doc(inline)]
pub use connection::{
    discover_hosts, host_subscription_stream, render_attach_command, spawn_attach_command,
    workspace_connection_stream, AttachCommandSpawner, AttachSpawnIntent, AttachTemplateValues,
};
#[doc(inline)]
pub use error::CoreError;
#[doc(inline)]
pub use link::{
    preview_action_prompt, preview_prompt_content, session_link_metadata, session_metadata_rows,
    MetadataRow, PromptContext, PromptLaunchParams, PromptPreview, ProviderLaunchItem,
    ProviderLaunchParams, SessionLinkKind, SessionLinkMetadata, SessionLinkProvider,
};
#[doc(inline)]
pub use message::DomainEvent;
#[doc(inline)]
pub use review::{
    default_reviews_dir, dispatch_review, new_review_id, parse_unified_diff, render_review_prompt,
    DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind, DiffModel, Review, ReviewComment,
    ReviewDispatchParams, ReviewId, ReviewLoadError, ReviewSide, ReviewSource, ReviewStatus,
    ReviewStore, ReviewStoreError, REVIEW_DISPATCHED_AT_KEY, REVIEW_SOURCE_KEY,
};
#[doc(inline)]
pub use sdk::{
    add_project, add_project_with_options, create_session, create_session_with_options,
    delete_notification, delete_notification_with_options, diff_session, diff_session_with_options,
    fork_session, fork_session_with_options, get_notification_policy_with_options, inspect_session,
    inspect_session_with_options, launch_action_prompt_with_options,
    launch_provider_item_with_options, list_notifications, list_notifications_with_options,
    list_project_actions, list_project_actions_with_options, list_projects,
    list_projects_with_options, load_host, load_host_snapshot, read_session_output,
    read_session_output_with_options, read_session_screen, read_session_screen_with_options,
    remove_project, remove_project_with_options, remove_session, remove_session_with_options,
    remove_worktree, remove_worktree_with_options, rename_project, rename_project_with_options,
    rename_session, rename_session_with_options, resolve_project_action,
    resolve_project_action_with_options, resolve_project_prompt,
    resolve_project_prompt_with_options, resume_session, resume_session_with_options,
    set_notification_policy_with_options, set_session_metadata, set_session_metadata_with_options,
    show_project, show_project_with_options, stop_session, stop_session_with_options,
    update_notification, update_notification_with_options, wait_for_session,
    wait_for_session_with_options,
};
#[doc(inline)]
pub use state::{
    AgentStateEvent, ConnState, GitHubProviderScope, GitHubProviderState,
    GitHubPullRequestStatusKey, HostEvent, HostView, LinearProviderState, NotificationFilter,
    NotificationIntent, NotificationRow, NotificationScope, PromptState, ProviderOperation,
    ProviderPanel, ProviderRequestId, ProviderState, ReviewCommentEditor, ReviewDiffStatus,
    ReviewDispatchModal, ReviewLineTarget, ReviewTabState, RuntimeContinuity, SessionAccess,
    SessionGroup, SessionObservation, SessionRow, Toast, Workspace,
};
#[doc(inline)]
pub use ui_state::{default_state_dir, Selection, TreeNodeId, UiState, UiStateError, WindowSize};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const UI_STATE_FILE: &str = "ui-state.toml";
const DEFAULT_LEFT_PANE_WIDTH: u16 = 280;
/// Stable protocol code older daemons return for unknown optional methods.
const METHOD_NOT_FOUND_CODE: &str = "method_not_found";
const DEFAULT_WINDOW_WIDTH: u32 = 960;
const DEFAULT_WINDOW_HEIGHT: u32 = 640;
/// Per-query page size used to seed the inbox from `notification.list` on
/// connect and reconcile.
///
/// The seed runs bounded queries for unread, live-default, and deleted
/// tombstone records (see [`notification_seed_queries`]), so recent unread
/// records and recent deletes are never crowded out by a long read/archive
/// history. It bounds reconcile cost while keeping realistic per-host inboxes
/// accurate.
const GUI_NOTIFICATION_SEED_LIMIT: u32 = 200;

/// Connection and reconciliation timing for host workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub reconcile_interval: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reconcile_interval: DEFAULT_RECONCILE_INTERVAL,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            backoff_max: DEFAULT_BACKOFF_MAX,
        }
    }
}

impl ConnectionOptions {
    fn client(self) -> ClientOptions {
        ClientOptions::default()
            .with_connect_timeout(self.connect_timeout)
            .with_request_timeout(self.request_timeout)
    }
}

/// Stable host key used by the GUI state and Iced subscription identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    /// Construct a host id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the stable host id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// SDK transport target for one host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HostTransport {
    /// Local daemon over Unix socket.
    Local { socket_path: PathBuf },
    /// Remote daemon resolved by the SDK through `NetBird`.
    Remote { host: String, socket_path: PathBuf },
    /// Remote daemon over a concrete TCP address.
    Tcp { addr: SocketAddr },
}

/// Static connection config for one host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostConfig {
    pub id: HostId,
    pub transport: HostTransport,
}

impl HostConfig {
    /// Build a local Unix-socket host config.
    #[must_use]
    pub fn local(id: impl Into<String>, socket_path: impl Into<PathBuf>) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Local {
                socket_path: socket_path.into(),
            },
        }
    }

    /// Build a TCP host config.
    #[must_use]
    pub fn tcp(id: impl Into<String>, addr: SocketAddr) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Tcp { addr },
        }
    }

    /// Build a remote host config resolved by the SDK.
    #[must_use]
    pub fn remote(
        id: impl Into<String>,
        host: impl Into<String>,
        socket_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Remote {
                host: host.into(),
                socket_path: socket_path.into(),
            },
        }
    }

    /// Value substituted into `{host}` for attach commands.
    #[must_use]
    pub fn attach_host(&self) -> &str {
        match &self.transport {
            HostTransport::Local { .. } => "",
            HostTransport::Remote { host, .. } => host,
            HostTransport::Tcp { .. } => self.id.as_str(),
        }
    }
}

/// Minimal daemon health facts used by the spike UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSummary {
    pub status: String,
    pub daemon_version: String,
    pub protocol_version: ProtocolVersion,
}

impl From<DaemonHealthResult> for HealthSummary {
    fn from(result: DaemonHealthResult) -> Self {
        Self {
            status: result.status,
            daemon_version: result.daemon_version,
            protocol_version: result.protocol_version,
        }
    }
}

/// A host snapshot seeded by `daemon.health`, `session.list`, `project.list`,
/// and `notification.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    pub host_id: HostId,
    pub health: HealthSummary,
    pub sessions: Vec<SessionInfo>,
    pub projects: Vec<ProjectInfo>,
    pub project_error: Option<String>,
    /// Recent notification records seeded from `notification.list`.
    ///
    /// Empty when the host daemon does not implement notifications; seeding is
    /// non-fatal so a host without the notification surface still connects.
    pub notifications: Vec<NotificationRecord>,
    /// Agent and profile names known to the daemon, seeded from `host.inspect`.
    ///
    /// This compatibility-oriented name list is not evidence that a runtime is
    /// installed or version-compatible. Launch decisions must use `runtimes`.
    pub supported_agents: Vec<String>,
    /// Full runtime inventory reported by `host.inspect`.
    pub runtimes: Vec<AgentRuntime>,
    /// Provider names reported by the host's runtime inventory.
    pub notification_providers: Vec<String>,
    /// Provider-neutral observation features advertised by `host.inspect`.
    pub observation_capabilities: ObservationCapabilities,
}

/// Returns whether a runtime can be selected for a new session.
///
/// Hermes requires positive version-policy confirmation. Legacy profiles that
/// predate `agent_base` remain selectable when available, while a future
/// unknown compiled base fails closed.
#[must_use]
pub fn runtime_is_launchable(runtime: &AgentRuntime) -> bool {
    if !runtime.available {
        return false;
    }

    match runtime.agent_base.as_ref() {
        Some(AgentKind::Hermes) => runtime.supported == Some(true),
        Some(AgentKind::Unknown(_)) => false,
        None if runtime.agent == "hermes" => runtime.supported == Some(true),
        Some(_) | None => true,
    }
}

/// Returns whether a launchable runtime can host the assistant.
///
/// Shell-backed profiles are excluded even when their profile name is not the
/// built-in `shell` name. Legacy custom profiles without `agent_base` retain
/// the name-based behavior used before the field existed.
#[must_use]
pub fn runtime_is_assistant_capable(runtime: &AgentRuntime) -> bool {
    runtime.agent != "shell"
        && runtime.agent_base.as_ref() != Some(&AgentKind::Shell)
        && runtime_is_launchable(runtime)
}

/// Host-level provider-neutral terminal observation capabilities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservationCapabilities {
    /// Whether the host can return a parsed terminal screen snapshot.
    pub terminal_read: bool,
    /// Whether the host can return raw session output by byte cursor.
    pub output_read: bool,
    /// Whether the host can wait for provider-neutral session predicates.
    pub session_wait: bool,
}
