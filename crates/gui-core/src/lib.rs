//! Headless state, SDK bridge logic, and command rendering for `pohunek-gui`.
//!
//! This crate intentionally has no Iced dependency. The native view layer wraps
//! these async helpers in Iced `Task` and `Subscription` values.

// Rust guideline compliant 2026-06-30
#![forbid(unsafe_code)]

pub mod assistant;
pub mod providers;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures::{stream, StreamExt};
use pohunek_client::{next_request_id, Client, ClientOptions};
use protocol::{
    event, method, AgentActivity, DaemonHealthResult, Event, HostClass, HostDiscoverParams,
    HostRecord, NotificationCreatedEvent, NotificationDeleteParams, NotificationDeleteResult,
    NotificationDeletedEvent, NotificationId, NotificationKind, NotificationListParams,
    NotificationListResult, NotificationRecord, NotificationSeverity, NotificationStatus,
    NotificationUpdateParams, NotificationUpdateResult, NotificationUpdatedEvent,
    ProjectActionParams, ProjectActionResult, ProjectActionsParams, ProjectActionsResult,
    ProjectAddParams, ProjectInfo, ProjectListParams, ProjectPromptParams, ProjectPromptResult,
    ProjectRemoveParams, ProjectRemoveResult, ProjectRenameParams, ProjectShowParams,
    ProjectShowResult, ProtocolVersion, ProviderKind, Request, SessionId, SessionInfo,
    SessionListParams, SessionNewParams, SessionNewResult, SessionRemoveResult,
    SessionRenameParams, SessionRenameResult, SessionResumeResult, SessionSetMetadataParams,
    SessionSetMetadataResult, SessionState, SessionStopResult, StateSource, WorktreeRemoveParams,
    WorktreeRemoveResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use pohunek_prompt::{
    render as render_prompt, Error as PromptError, Provider as PromptProvider,
};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const UI_STATE_FILE: &str = "ui-state.toml";
const DEFAULT_LEFT_PANE_WIDTH: u16 = 280;
/// Stable protocol code older daemons return for unknown optional methods.
const METHOD_NOT_FOUND_CODE: &str = "method_not_found";
/// Minimum height for the Agents monitor.
///
/// This leaves room for about five compact two-line session rows in the
/// default-height window; the previous 220px layout fit only about three.
const MIN_AGENTS_PANE_HEIGHT: u16 = 360;
const DEFAULT_AGENTS_PANE_HEIGHT: u16 = MIN_AGENTS_PANE_HEIGHT;
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
}

/// Provider context used to render a prompt preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptContext {
    pub provider: PromptProvider,
    pub item_id: String,
    pub json: String,
}

/// Rendered prompt preview ready for launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPreview {
    pub prompt_name: String,
    pub rendered: String,
    pub branch: Option<String>,
}

/// Launch request for a rendered project action prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLaunchParams {
    pub project: String,
    pub action: ProjectActionResult,
    pub preview: PromptPreview,
    pub cols: u16,
    pub rows: u16,
    pub metadata: BTreeMap<String, String>,
    /// Owner-set display name for the launched session, or `None` for id-only.
    pub name: Option<String>,
}

/// Provider session link owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkProvider {
    /// Linear issue provider.
    Linear,
    /// GitHub provider.
    GitHub,
}

impl SessionLinkProvider {
    /// Stable metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::GitHub => "github",
        }
    }

    const fn from_metadata(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"linear" => Some(Self::Linear),
            b"github" => Some(Self::GitHub),
            _ => None,
        }
    }
}

/// Provider item kind stored in session link metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkKind {
    /// Issue work item.
    Issue,
    /// Pull request work item.
    PullRequest,
}

impl SessionLinkKind {
    /// Stable metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull_request",
        }
    }

    const fn from_metadata(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"issue" => Some(Self::Issue),
            b"pull_request" => Some(Self::PullRequest),
            _ => None,
        }
    }
}

/// Opaque provider link metadata written at `session.new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLinkMetadata {
    pub provider: SessionLinkProvider,
    pub kind: SessionLinkKind,
    pub id: String,
    pub url: String,
    pub branch: String,
}

impl SessionLinkMetadata {
    /// Creates validated link metadata.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when an opaque link value is empty.
    pub fn new(
        provider: SessionLinkProvider,
        kind: SessionLinkKind,
        id: impl Into<String>,
        url: impl Into<String>,
        branch: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let link = Self {
            provider,
            kind,
            id: id.into(),
            url: url.into(),
            branch: branch.into(),
        };
        link.validate()?;
        Ok(link)
    }

    /// Returns metadata keys accepted by `session.new`.
    #[must_use]
    pub fn to_session_metadata(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "link.provider".to_owned(),
                self.provider.as_str().to_owned(),
            ),
            ("link.kind".to_owned(), self.kind.as_str().to_owned()),
            ("link.id".to_owned(), self.id.clone()),
            ("link.url".to_owned(), self.url.clone()),
            ("link.branch".to_owned(), self.branch.clone()),
        ])
    }

    fn validate(&self) -> Result<(), CoreError> {
        checked_link_value("link.id", self.id.clone())?;
        checked_link_value("link.url", self.url.clone())?;
        checked_link_value("link.branch", self.branch.clone())?;
        Ok(())
    }
}

fn checked_link_value(field: &'static str, value: String) -> Result<String, CoreError> {
    if value.trim().is_empty() {
        Err(CoreError::MissingLinkField { field })
    } else {
        Ok(value)
    }
}

/// Provider item context used to resolve, render, launch, and link a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchItem {
    action_provider: ProviderKind,
    prompt_provider: PromptProvider,
    item_id: String,
    context_json: String,
    link_provider: SessionLinkProvider,
    link_kind: SessionLinkKind,
    link_url: String,
}

/// Provider launch request that resolves a project action before `session.new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchParams {
    pub project: String,
    pub action_name: String,
    pub item: ProviderLaunchItem,
    pub cols: u16,
    pub rows: u16,
    /// Owner-set display name for the launched session, or `None` for id-only.
    pub name: Option<String>,
}

impl ProviderLaunchItem {
    /// Builds a launch context for a Linear issue.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when required link metadata is empty.
    pub fn linear_issue(
        item_id: impl Into<String>,
        context_json: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let item_id = checked_link_value("link.id", item_id.into())?;
        Ok(Self {
            action_provider: ProviderKind::LinearIssue,
            prompt_provider: PromptProvider::LinearIssue,
            link_provider: SessionLinkProvider::Linear,
            link_kind: SessionLinkKind::Issue,
            link_url: checked_link_value("link.url", url.into())?,
            item_id,
            context_json: context_json.into(),
        })
    }

    /// Builds a launch context for a GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when required link metadata is empty.
    pub fn github_pull_request(
        number: impl Into<String>,
        context_json: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let number = checked_link_value("link.id", number.into())?;
        Ok(Self {
            action_provider: ProviderKind::GithubPr,
            prompt_provider: PromptProvider::GitHubPr,
            link_provider: SessionLinkProvider::GitHub,
            link_kind: SessionLinkKind::PullRequest,
            link_url: checked_link_value("link.url", url.into())?,
            item_id: number,
            context_json: context_json.into(),
        })
    }

    fn validate_link_invariants(&self) -> Result<(), CoreError> {
        let expected = match (
            &self.action_provider,
            self.prompt_provider,
            self.link_provider,
            self.link_kind,
        ) {
            (
                ProviderKind::LinearIssue,
                PromptProvider::LinearIssue,
                SessionLinkProvider::Linear,
                SessionLinkKind::Issue,
            )
            | (
                ProviderKind::GithubPr,
                PromptProvider::GitHubPr,
                SessionLinkProvider::GitHub,
                SessionLinkKind::PullRequest,
            ) => return Ok(()),
            _ => "action provider, prompt provider, and link metadata must describe the same provider item",
        };
        Err(CoreError::ProviderLaunchItemMismatch { message: expected })
    }

    fn to_session_link(&self, branch: impl Into<String>) -> Result<SessionLinkMetadata, CoreError> {
        SessionLinkMetadata::new(
            self.link_provider,
            self.link_kind,
            self.item_id.clone(),
            self.link_url.clone(),
            branch,
        )
    }
}

/// Prompt/action browse and preview state for one host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptState {
    pub actions_by_project: BTreeMap<String, ProjectActionsResult>,
    pub resolved_prompt: Option<ProjectPromptResult>,
    pub resolved_action: Option<ProjectActionResult>,
    pub preview: Option<PromptPreview>,
}

/// Active provider browser panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderPanel {
    /// Linear issues.
    #[default]
    Linear,
    /// GitHub issues and pull requests.
    GitHub,
}

/// Provider browser state owned by gui-core.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderState {
    pub active_panel: ProviderPanel,
    pub linear: LinearProviderState,
    pub github: GitHubProviderState,
}

/// Monotonic id for one provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderRequestId(u64);

impl ProviderRequestId {
    /// Borrow the numeric request id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Provider operation used to reject stale async completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOperation {
    /// Linear assigned issue fetch.
    LinearIssues,
    /// GitHub pull request list fetch.
    GitHubPullRequests,
    /// GitHub issue list fetch.
    GitHubIssues,
    /// GitHub PR status fetch.
    GitHubPullRequestStatus,
    /// Provider launch action.
    Launch,
}

/// Linear provider browser state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinearProviderState {
    /// Name of the picked predefined filter; `None` until one is chosen.
    pub selected_filter: Option<String>,
    pub search: String,
    pub issues: Vec<providers::linear::LinearIssue>,
    pub selected_issue_id: Option<String>,
    pub active_request: Option<ProviderRequestId>,
    pub last_error: Option<String>,
}

/// GitHub provider browser state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitHubProviderState {
    pub scope: Option<GitHubProviderScope>,
    /// Name of the picked predefined pull request filter; `None` until chosen.
    pub selected_filter: Option<String>,
    pub search: String,
    pub pull_requests: Vec<providers::github::GitHubPullRequest>,
    pub issues: Vec<providers::github::GitHubIssue>,
    pub selected_pull_request: Option<u64>,
    pub selected_issue: Option<u64>,
    pub pull_requests_request: Option<ProviderRequestId>,
    pub issues_request: Option<ProviderRequestId>,
    pub pull_request_status_request: Option<ProviderRequestId>,
    pub pull_request_statuses:
        BTreeMap<GitHubPullRequestStatusKey, providers::github::PullRequestStatus>,
    pub last_error: Option<String>,
}

/// GitHub repository scope for provider data loaded through `gh`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitHubProviderScope {
    pub project_id: String,
    pub repo_root: PathBuf,
}

impl GitHubProviderScope {
    /// Construct a GitHub provider scope from the selected project identity.
    #[must_use]
    pub fn new(project_id: impl Into<String>, repo_root: impl Into<PathBuf>) -> Self {
        Self {
            project_id: project_id.into(),
            repo_root: repo_root.into(),
        }
    }

    /// Construct a GitHub provider scope from a daemon project record.
    #[must_use]
    pub fn from_project(project: &ProjectInfo) -> Self {
        Self::new(project.id.clone(), project.repo_root.clone())
    }
}

/// Cache key for GitHub PR status loaded for a specific repository scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitHubPullRequestStatusKey {
    pub scope: GitHubProviderScope,
    pub url: String,
}

impl GitHubPullRequestStatusKey {
    /// Construct a PR status cache key.
    #[must_use]
    pub fn new(scope: GitHubProviderScope, url: impl Into<String>) -> Self {
        Self {
            scope,
            url: url.into(),
        }
    }
}

/// Parsed `agent_state` event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStateEvent {
    pub session_id: SessionId,
    pub activity: AgentActivity,
    pub source: StateSource,
    pub raw: Event,
}

/// Protocol events surfaced by a host subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    AgentState(AgentStateEvent),
    SessionCreated(SessionInfo),
    SessionUpdated(SessionInfo),
    SessionStopped(SessionInfo),
    SessionRemoved(SessionInfo),
    /// A durable notification record was created on the host.
    NotificationCreated(NotificationRecord),
    /// A durable notification record changed lifecycle status or content.
    NotificationUpdated(NotificationRecord),
    /// A durable notification record was hard-deleted on the host.
    NotificationDeleted(NotificationId),
    Other(Event),
}

/// Message emitted by async host workers and applied to [`Workspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    HostConnecting {
        host_id: HostId,
    },
    HostSnapshotLoaded {
        snapshot: HostSnapshot,
    },
    HostSubscribed {
        host_id: HostId,
    },
    HostEvent {
        host_id: HostId,
        event: HostEvent,
    },
    HostDisconnected {
        host_id: HostId,
        error: String,
    },
    HostUnreachable {
        host_id: HostId,
        error: String,
    },
    SessionCreated {
        host_id: HostId,
        session: SessionInfo,
    },
    SessionInspected {
        host_id: HostId,
        session: SessionInfo,
    },
    SessionResumed {
        host_id: HostId,
        result: SessionResumeResult,
    },
    SessionStopCompleted {
        host_id: HostId,
        session_id: SessionId,
        result: SessionStopResult,
    },
    SessionRemoveCompleted {
        host_id: HostId,
        session_id: SessionId,
        result: SessionRemoveResult,
    },
    SessionMetadataUpdated {
        host_id: HostId,
        result: SessionSetMetadataResult,
    },
    SessionRenamed {
        host_id: HostId,
        result: SessionRenameResult,
    },
    ProjectListLoaded {
        host_id: HostId,
        projects: Vec<ProjectInfo>,
    },
    ProjectAdded {
        host_id: HostId,
        project: ProjectInfo,
    },
    ProjectShown {
        host_id: HostId,
        result: ProjectShowResult,
    },
    ProjectRenamed {
        host_id: HostId,
        project: ProjectInfo,
    },
    ProjectRemoved {
        host_id: HostId,
        reference: String,
        result: ProjectRemoveResult,
    },
    WorktreeRemoved {
        host_id: HostId,
        project_id: String,
        path: PathBuf,
        result: WorktreeRemoveResult,
    },
    ProjectActionsLoaded {
        host_id: HostId,
        reference: String,
        result: ProjectActionsResult,
    },
    ProjectPromptResolved {
        host_id: HostId,
        prompt: ProjectPromptResult,
    },
    ProjectActionResolved {
        host_id: HostId,
        action: ProjectActionResult,
    },
    PromptPreviewRendered {
        host_id: HostId,
        preview: PromptPreview,
    },
    ProviderPanelSelected {
        host_id: HostId,
        panel: ProviderPanel,
    },
    LinearProviderFilterSelected {
        host_id: HostId,
        name: String,
    },
    LinearProviderSearchChanged {
        host_id: HostId,
        value: String,
    },
    LinearProviderIssuesLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        filter_name: Option<String>,
        search: String,
        issues: Vec<providers::linear::LinearIssue>,
    },
    LinearProviderIssueSelected {
        host_id: HostId,
        issue_id: String,
    },
    GitHubProviderFilterSelected {
        host_id: HostId,
        name: String,
    },
    GitHubProviderSearchChanged {
        host_id: HostId,
        value: String,
    },
    GitHubProviderPullRequestsLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        scope: GitHubProviderScope,
        pull_requests: Vec<providers::github::GitHubPullRequest>,
    },
    GitHubProviderIssuesLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        scope: GitHubProviderScope,
        issues: Vec<providers::github::GitHubIssue>,
    },
    GitHubProviderPullRequestSelected {
        host_id: HostId,
        number: u64,
    },
    GitHubProviderIssueSelected {
        host_id: HostId,
        number: u64,
    },
    GitHubProviderPullRequestStatusLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        status_key: GitHubPullRequestStatusKey,
        status: providers::github::PullRequestStatus,
    },
    ProviderOperationFailed {
        host_id: HostId,
        provider: SessionLinkProvider,
        operation: ProviderOperation,
        request_id: Option<ProviderRequestId>,
        error: String,
    },
    HostOperationFailed {
        host_id: HostId,
        error: String,
    },
    NotificationUpdateCompleted {
        host_id: HostId,
        result: NotificationUpdateResult,
    },
    NotificationDeleteCompleted {
        host_id: HostId,
        result: NotificationDeleteResult,
    },
}

/// Per-host connection state for the headless workspace model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
    Unreachable,
}

/// Active detail selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    Host {
        host_id: HostId,
    },
    Project {
        host_id: HostId,
        project_id: String,
    },
    Session {
        host_id: HostId,
        session_id: SessionId,
    },
    Notification {
        host_id: HostId,
        notification_id: NotificationId,
    },
}

/// Persisted expanded workspace tree node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeNodeId {
    Host { host_id: HostId },
    Project { host_id: HostId, project_id: String },
}

impl TreeNodeId {
    /// Construct a host node id.
    #[must_use]
    pub fn host(host_id: HostId) -> Self {
        Self::Host { host_id }
    }

    /// Construct a project node id.
    #[must_use]
    pub fn project(host_id: HostId, project_id: impl Into<String>) -> Self {
        Self::Project {
            host_id,
            project_id: project_id.into(),
        }
    }
}

/// Detail tabs restored by the GUI shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailTab {
    Session,
    Agents,
    Project,
}

/// Persisted window dimensions.
///
/// These remain `u32` for compatibility with existing TOML state; the Iced
/// shell clamps values to the platform window range when converting to/from
/// floating-point pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// Persisted UI layout and selection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiState {
    pub left_pane_width: u16,
    pub agents_pane_height: u16,
    pub window_size: WindowSize,
    pub expanded_nodes: BTreeSet<TreeNodeId>,
    pub selection: Option<Selection>,
    pub open_tabs: Vec<DetailTab>,
    pub active_tab: DetailTab,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            left_pane_width: DEFAULT_LEFT_PANE_WIDTH,
            agents_pane_height: DEFAULT_AGENTS_PANE_HEIGHT,
            window_size: WindowSize {
                width: DEFAULT_WINDOW_WIDTH,
                height: DEFAULT_WINDOW_HEIGHT,
            },
            expanded_nodes: BTreeSet::new(),
            selection: None,
            open_tabs: vec![DetailTab::Session, DetailTab::Agents],
            active_tab: DetailTab::Session,
        }
    }
}

impl UiState {
    /// Load persisted UI state from `dir`.
    ///
    /// A missing state file restores defaults; malformed state returns an error
    /// so the shell can surface it instead of silently discarding operator state.
    pub fn load_from_dir(dir: impl AsRef<std::path::Path>) -> Result<Self, UiStateError> {
        let path = dir.as_ref().join(UI_STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let state = toml::from_str(&raw).map_err(|source| UiStateError::Parse {
                    path: path.clone(),
                    source,
                })?;
                Ok(normalize_loaded_ui_state(state))
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(UiStateError::Read { path, source }),
        }
    }

    /// Save UI state to `dir`.
    pub fn save_to_dir(&self, dir: impl AsRef<std::path::Path>) -> Result<(), UiStateError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|source| UiStateError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(UI_STATE_FILE);
        let raw = toml::to_string_pretty(self).map_err(UiStateError::Serialize)?;
        std::fs::write(&path, raw).map_err(|source| UiStateError::Write { path, source })
    }
}

fn normalize_loaded_ui_state(mut state: UiState) -> UiState {
    state.agents_pane_height = state.agents_pane_height.max(MIN_AGENTS_PANE_HEIGHT);
    state
}

/// Errors raised while loading or saving persistent UI state.
#[derive(Debug, Error)]
pub enum UiStateError {
    #[error("failed to create UI state directory `{}`: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read UI state `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse UI state `{}`: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize UI state: {0}")]
    Serialize(toml::ser::Error),
    #[error("failed to write UI state `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: &'static str },
}

/// Return the default XDG state directory for `pohunek-gui`.
pub fn default_state_dir() -> Result<PathBuf, UiStateError> {
    if let Ok(value) = std::env::var("XDG_STATE_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value).join("pohunek-gui"));
        }
    }
    match std::env::var("HOME") {
        Ok(value) if !value.is_empty() => Ok(PathBuf::from(value)
            .join(".local")
            .join("state")
            .join("pohunek-gui")),
        _ => Err(UiStateError::MissingEnv { var: "HOME" }),
    }
}

/// OS notification requested by core state transitions.
///
/// Raised from durable `notification_created` events whose severity warrants an
/// immediate desktop notification (see [`notification_raises_intent`]). The
/// monotonic `id` lets the shell consume new intents with a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationIntent {
    pub id: u64,
    pub host_id: HostId,
    /// Durable notification that raised this intent, so the shell can open its
    /// inbox detail on click.
    pub notification_id: Option<NotificationId>,
    /// Linked session, when the source notification is bound to one.
    pub session_id: Option<SessionId>,
    pub title: String,
    pub body: String,
}

/// In-app notification requested by core state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub host_id: HostId,
    pub session_id: SessionId,
    pub message: String,
}

/// Filter for the inbox notification-list selectors.
///
/// Every `Some` field must match; `None` fields do not constrain the result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationFilter {
    pub host_id: Option<HostId>,
    pub status: Option<NotificationStatus>,
    pub severity: Option<NotificationSeverity>,
    pub kind: Option<NotificationKind>,
    pub provider: Option<String>,
}

impl NotificationFilter {
    /// Whether `record` passes the non-host constraints of this filter.
    ///
    /// The host constraint is applied by the caller, which already iterates
    /// hosts and can skip whole hosts without inspecting their records.
    fn matches(&self, record: &NotificationRecord) -> bool {
        if self.status.is_some_and(|status| status != record.status) {
            return false;
        }
        if self
            .severity
            .is_some_and(|severity| severity != record.severity)
        {
            return false;
        }
        if self.kind.is_some_and(|kind| kind != record.kind) {
            return false;
        }
        if self
            .provider
            .as_ref()
            .is_some_and(|provider| provider != &record.source.provider)
        {
            return false;
        }
        true
    }
}

/// One notification row for the inbox list, tagged with its owning host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationRow {
    pub host_id: HostId,
    pub record: NotificationRecord,
}

/// GUI-facing state for one daemon host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    pub conn: ConnState,
    pub health: Option<HealthSummary>,
    pub sessions: BTreeMap<String, SessionInfo>,
    pub projects: BTreeMap<String, ProjectInfo>,
    pub project_details: BTreeMap<String, ProjectShowResult>,
    pub prompt: PromptState,
    pub provider: ProviderState,
    /// Durable notification records for this host, keyed by notification id.
    pub notifications: BTreeMap<String, NotificationRecord>,
    pub last_agent_state: Option<AgentStateEvent>,
    pub last_error: Option<String>,
}

impl HostView {
    fn connecting() -> Self {
        Self {
            conn: ConnState::Connecting,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            project_details: BTreeMap::new(),
            prompt: PromptState::default(),
            provider: ProviderState::default(),
            notifications: BTreeMap::new(),
            last_agent_state: None,
            last_error: None,
        }
    }
}

/// Headless workspace model owned by `gui-core`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub hosts: BTreeMap<HostId, HostView>,
    pub selection: Option<Selection>,
    pub notification_intents: Vec<NotificationIntent>,
    pub toasts: Vec<Toast>,
    next_intent_id: u64,
    next_provider_request_id: u64,
}

impl Workspace {
    fn next_provider_request_id(&mut self) -> ProviderRequestId {
        self.next_provider_request_id = self.next_provider_request_id.saturating_add(1);
        ProviderRequestId(self.next_provider_request_id)
    }

    /// Mark a Linear issue fetch as the current request for `host_id`.
    pub fn begin_linear_issues_request(&mut self, host_id: HostId) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.linear.active_request = Some(request_id);
        host.provider.linear.last_error = None;
        request_id
    }

    /// Mark a GitHub pull request list fetch as the current request for `host_id`.
    pub fn begin_github_pull_requests_request(&mut self, host_id: HostId) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.github.pull_requests_request = Some(request_id);
        host.provider.github.last_error = None;
        request_id
    }

    /// Mark a GitHub issue list fetch as the current request for `host_id`.
    pub fn begin_github_issues_request(&mut self, host_id: HostId) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.github.issues_request = Some(request_id);
        host.provider.github.last_error = None;
        request_id
    }

    /// Mark a GitHub pull request status fetch as the current request for `host_id`.
    pub fn begin_github_pull_request_status_request(
        &mut self,
        host_id: HostId,
    ) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.github.pull_request_status_request = Some(request_id);
        host.provider.github.last_error = None;
        request_id
    }

    /// Invalidate pending GitHub provider requests for `host_id`.
    pub fn invalidate_github_provider_requests(&mut self, host_id: &HostId) {
        if let Some(host) = self.hosts.get_mut(host_id) {
            host.provider.github.pull_requests_request = None;
            host.provider.github.issues_request = None;
            host.provider.github.pull_request_status_request = None;
        }
    }

    fn selected_github_scope(&self, host_id: &HostId) -> Option<GitHubProviderScope> {
        match self.selection.as_ref()? {
            Selection::Project {
                host_id: selected_host,
                project_id,
            } if selected_host == host_id => self
                .hosts
                .get(host_id)?
                .projects
                .get(project_id)
                .map(GitHubProviderScope::from_project),
            Selection::Session {
                host_id: selected_host,
                session_id,
            } if selected_host == host_id => {
                let host = self.hosts.get(host_id)?;
                let project_id = host.sessions.get(&session_id.0)?.project_id.as_ref()?;
                host.projects
                    .get(project_id)
                    .map(GitHubProviderScope::from_project)
            }
            _ => None,
        }
    }

    /// Apply one async message to the workspace state.
    #[expect(
        clippy::too_many_lines,
        reason = "workspace updates are centralized so GUI transitions stay deterministic and testable"
    )]
    pub fn apply(&mut self, message: Message) {
        match message {
            Message::HostConnecting { host_id } => {
                self.hosts
                    .entry(host_id)
                    .and_modify(|host| {
                        host.conn = ConnState::Connecting;
                        host.last_error = None;
                    })
                    .or_insert_with(HostView::connecting);
            }
            Message::HostSnapshotLoaded { snapshot } => {
                let host_id = snapshot.host_id.clone();
                let sessions = snapshot
                    .sessions
                    .iter()
                    .cloned()
                    .map(|session| (session.id.0.clone(), session))
                    .collect();
                let projects = snapshot
                    .projects
                    .iter()
                    .cloned()
                    .map(|project| (project.id.clone(), project))
                    .collect();
                let previous_details = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(BTreeMap::new, |host| host.project_details.clone());
                let previous_prompt = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(PromptState::default, |host| host.prompt.clone());
                let previous_provider = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(ProviderState::default, |host| host.provider.clone());
                // Notifications reconcile by merge, not wholesale replacement: the
                // seed query returns a bounded recent window, so previously
                // received records are preserved and missed records are folded in
                // (deduped by id). Seeding never raises OS intents.
                let mut notifications = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(BTreeMap::new, |host| host.notifications.clone());
                for record in snapshot.notifications {
                    upsert_notification(&mut notifications, record);
                }
                self.hosts.insert(
                    snapshot.host_id,
                    HostView {
                        conn: ConnState::Connected,
                        health: Some(snapshot.health),
                        sessions,
                        projects,
                        project_details: previous_details,
                        prompt: previous_prompt,
                        provider: previous_provider,
                        notifications,
                        last_agent_state: None,
                        last_error: snapshot.project_error,
                    },
                );
            }
            Message::HostSubscribed { host_id } => {
                self.hosts
                    .entry(host_id)
                    .and_modify(|host| {
                        host.conn = ConnState::Connected;
                        host.last_error = None;
                    })
                    .or_insert_with(|| HostView {
                        conn: ConnState::Connected,
                        ..HostView::connecting()
                    });
            }
            Message::HostEvent { host_id, event } => {
                let Some(host) = self.hosts.get_mut(&host_id) else {
                    trace_ignored_unknown_host(&host_id, "host event");
                    return;
                };
                host.conn = ConnState::Connected;
                host.last_error = None;
                apply_host_event(
                    host,
                    &host_id,
                    event,
                    &mut self.notification_intents,
                    &mut self.toasts,
                    &mut self.next_intent_id,
                );
            }
            Message::HostDisconnected { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "host disconnected") else {
                    return;
                };
                host.conn = ConnState::Disconnected;
                host.last_error = Some(error);
            }
            Message::HostUnreachable { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "host unreachable") else {
                    return;
                };
                host.conn = ConnState::Unreachable;
                host.last_error = Some(error);
            }
            Message::SessionCreated { host_id, session }
            | Message::SessionInspected { host_id, session } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session result") else {
                    return;
                };
                host.sessions.insert(session.id.0.clone(), session);
            }
            Message::SessionResumed { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session resume result") else {
                    return;
                };
                let session = result.session;
                host.sessions.insert(session.id.0.clone(), session);
            }
            Message::SessionStopCompleted {
                host_id,
                session_id,
                result,
            } => {
                if !result.stopped {
                    return;
                }
                let Some(host) = self.host_mut_if_known(&host_id, "session stop result") else {
                    return;
                };
                if let Some(session) = host.sessions.get_mut(&session_id.0) {
                    session.state = SessionState::Stopped;
                    session.activity = None;
                }
            }
            Message::SessionRemoveCompleted {
                host_id,
                session_id,
                result,
            } => {
                if !result.removed {
                    return;
                }
                let Some(host) = self.host_mut_if_known(&host_id, "session remove result") else {
                    return;
                };
                host.sessions.remove(&session_id.0);
            }
            Message::SessionMetadataUpdated { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session metadata result") else {
                    return;
                };
                host.sessions
                    .insert(result.session.id.0.clone(), result.session);
            }
            Message::SessionRenamed { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session renamed result") else {
                    return;
                };
                host.sessions
                    .insert(result.session.id.0.clone(), result.session);
            }
            Message::ProjectListLoaded { host_id, projects } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project list result") else {
                    return;
                };
                host.projects = projects
                    .iter()
                    .cloned()
                    .map(|project| (project.id.clone(), project))
                    .collect();
                host.project_details
                    .retain(|id, _details| host.projects.contains_key(id));
            }
            Message::ProjectAdded { host_id, project }
            | Message::ProjectRenamed { host_id, project } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project change result") else {
                    return;
                };
                host.projects.insert(project.id.clone(), project);
            }
            Message::ProjectShown { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project shown result") else {
                    return;
                };
                host.projects
                    .insert(result.project.id.clone(), result.project.clone());
                host.project_details
                    .insert(result.project.id.clone(), result);
            }
            Message::ProjectRemoved {
                host_id,
                reference,
                result,
            } => {
                if !result.removed {
                    return;
                }
                if let Some(host) = self.hosts.get_mut(&host_id) {
                    let removed_ids = host
                        .projects
                        .iter()
                        .filter(|(id, project)| *id == &reference || project.label == reference)
                        .map(|(id, _project)| id.clone())
                        .collect::<Vec<_>>();
                    for id in removed_ids {
                        host.projects.remove(&id);
                        host.project_details.remove(&id);
                    }
                }
            }
            Message::WorktreeRemoved {
                host_id,
                project_id,
                path,
                result,
            } => {
                if !result.removed {
                    return;
                }
                // Drop the removed worktree from the cached project detail so the
                // row disappears immediately, without waiting for a refresh.
                if let Some(details) = self
                    .hosts
                    .get_mut(&host_id)
                    .and_then(|host| host.project_details.get_mut(&project_id))
                {
                    details.worktrees.retain(|worktree| worktree.path != path);
                }
            }
            Message::ProjectActionsLoaded {
                host_id,
                reference,
                result,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project actions result") else {
                    return;
                };
                host.prompt.actions_by_project.insert(reference, result);
                host.last_error = None;
            }
            Message::ProjectPromptResolved { host_id, prompt } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project prompt result") else {
                    return;
                };
                host.prompt.resolved_prompt = Some(prompt);
                host.prompt.preview = None;
                host.last_error = None;
            }
            Message::ProjectActionResolved { host_id, action } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project action result") else {
                    return;
                };
                host.prompt.resolved_action = Some(action);
                host.prompt.preview = None;
                host.last_error = None;
            }
            Message::PromptPreviewRendered { host_id, preview } => {
                let Some(host) = self.host_mut_if_known(&host_id, "prompt preview result") else {
                    return;
                };
                host.prompt.preview = Some(preview);
                host.last_error = None;
            }
            Message::ProviderPanelSelected { host_id, panel } => {
                let host = self.host_for_ui(host_id);
                host.provider.active_panel = panel;
            }
            Message::LinearProviderFilterSelected { host_id, name } => {
                let host = self.host_for_ui(host_id);
                host.provider.linear.selected_filter = Some(name);
                host.provider.linear.active_request = None;
            }
            Message::LinearProviderSearchChanged { host_id, value } => {
                let host = self.host_for_ui(host_id);
                host.provider.linear.search = value;
                host.provider.linear.active_request = None;
            }
            Message::LinearProviderIssuesLoaded {
                host_id,
                request_id,
                filter_name,
                search,
                issues,
            } => {
                let trace_host_id = host_id.clone();
                let Some(host) = self.host_mut_if_known(&host_id, "linear issues result") else {
                    return;
                };
                if host.provider.linear.active_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::Linear,
                        ProviderOperation::LinearIssues,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                if host.provider.linear.selected_filter != filter_name
                    || host.provider.linear.search != search
                {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::Linear,
                        ProviderOperation::LinearIssues,
                        request_id,
                        "filter_changed",
                    );
                    return;
                }
                host.provider.linear.active_request = None;
                host.provider.linear.issues = issues;
                host.provider.linear.selected_issue_id = None;
                host.provider.linear.last_error = None;
            }
            Message::LinearProviderIssueSelected { host_id, issue_id } => {
                let host = self.host_for_ui(host_id);
                host.provider.linear.selected_issue_id = Some(issue_id);
            }
            Message::GitHubProviderFilterSelected { host_id, name } => {
                let host = self.host_for_ui(host_id);
                host.provider.github.selected_filter = Some(name);
                host.provider.github.pull_requests_request = None;
            }
            Message::GitHubProviderSearchChanged { host_id, value } => {
                let host = self.host_for_ui(host_id);
                host.provider.github.search = value;
            }
            Message::GitHubProviderPullRequestsLoaded {
                host_id,
                request_id,
                scope,
                pull_requests,
            } => {
                if self
                    .selected_github_scope(&host_id)
                    .as_ref()
                    .is_some_and(|current| current != &scope)
                {
                    trace_ignored_provider_result(
                        &host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequests,
                        request_id,
                        "selection_changed",
                    );
                    return;
                }
                let trace_host_id = host_id.clone();
                let Some(host) = self.host_mut_if_known(&host_id, "github pull requests result")
                else {
                    return;
                };
                if host.provider.github.pull_requests_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequests,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                host.provider.github.pull_requests_request = None;
                if host.provider.github.scope.as_ref() != Some(&scope) {
                    host.provider.github.issues.clear();
                    host.provider.github.selected_issue = None;
                }
                host.provider.github.scope = Some(scope);
                host.provider.github.pull_requests = pull_requests;
                host.provider.github.selected_pull_request = None;
                host.provider.github.last_error = None;
            }
            Message::GitHubProviderIssuesLoaded {
                host_id,
                request_id,
                scope,
                issues,
            } => {
                if self
                    .selected_github_scope(&host_id)
                    .as_ref()
                    .is_some_and(|current| current != &scope)
                {
                    trace_ignored_provider_result(
                        &host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubIssues,
                        request_id,
                        "selection_changed",
                    );
                    return;
                }
                let trace_host_id = host_id.clone();
                let Some(host) = self.host_mut_if_known(&host_id, "github issues result") else {
                    return;
                };
                if host.provider.github.issues_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubIssues,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                host.provider.github.issues_request = None;
                if host.provider.github.scope.as_ref() != Some(&scope) {
                    host.provider.github.pull_requests.clear();
                    host.provider.github.selected_pull_request = None;
                }
                host.provider.github.scope = Some(scope);
                host.provider.github.issues = issues;
                host.provider.github.selected_issue = None;
                host.provider.github.last_error = None;
            }
            Message::GitHubProviderPullRequestSelected { host_id, number } => {
                let host = self.host_for_ui(host_id);
                host.provider.github.selected_pull_request = Some(number);
            }
            Message::GitHubProviderIssueSelected { host_id, number } => {
                let host = self.host_for_ui(host_id);
                host.provider.github.selected_issue = Some(number);
            }
            Message::GitHubProviderPullRequestStatusLoaded {
                host_id,
                request_id,
                status_key,
                status,
            } => {
                if self
                    .selected_github_scope(&host_id)
                    .as_ref()
                    .is_some_and(|current| current != &status_key.scope)
                {
                    trace_ignored_provider_result(
                        &host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequestStatus,
                        request_id,
                        "selection_changed",
                    );
                    return;
                }
                let trace_host_id = host_id.clone();
                let Some(host) =
                    self.host_mut_if_known(&host_id, "github pull request status result")
                else {
                    return;
                };
                if host.provider.github.pull_request_status_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequestStatus,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                host.provider.github.pull_request_status_request = None;
                host.provider
                    .github
                    .pull_request_statuses
                    .insert(status_key, status);
                host.provider.github.last_error = None;
            }
            Message::ProviderOperationFailed {
                host_id,
                provider,
                operation,
                request_id,
                error,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "provider operation failure")
                else {
                    return;
                };
                if !apply_provider_request_failure(&host_id, host, provider, operation, request_id)
                {
                    return;
                }
                match provider {
                    SessionLinkProvider::Linear => host.provider.linear.last_error = Some(error),
                    SessionLinkProvider::GitHub => host.provider.github.last_error = Some(error),
                }
            }
            Message::HostOperationFailed { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "host operation failure") else {
                    return;
                };
                host.last_error = Some(error);
            }
            Message::NotificationUpdateCompleted { host_id, result } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                upsert_notification(&mut host.notifications, result.record);
            }
            Message::NotificationDeleteCompleted { host_id, result } => {
                if !result.deleted {
                    return;
                }
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                remove_notification(&mut host.notifications, &result.id);
            }
        }
    }

    fn host_mut_if_known(
        &mut self,
        host_id: &HostId,
        reason: &'static str,
    ) -> Option<&mut HostView> {
        let Some(host) = self.hosts.get_mut(host_id) else {
            trace_ignored_unknown_host(host_id, reason);
            return None;
        };
        Some(host)
    }

    fn host_for_ui(&mut self, host_id: HostId) -> &mut HostView {
        self.hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting)
    }

    /// Select a session in the detail pane.
    pub fn select_session(&mut self, host_id: HostId, session_id: SessionId) {
        self.invalidate_github_provider_requests(&host_id);
        self.selection = Some(Selection::Session {
            host_id,
            session_id,
        });
    }

    /// Select a project in the detail pane.
    pub fn select_project(&mut self, host_id: HostId, project_id: String) {
        self.invalidate_github_provider_requests(&host_id);
        self.selection = Some(Selection::Project {
            host_id,
            project_id,
        });
    }

    /// Select a notification, following its linked session when that session is
    /// still live.
    ///
    /// A notification bound to a session the host still knows selects that
    /// session so the operator lands on the live work. When the session no
    /// longer exists (or the notification is not session-bound), notification
    /// detail opens instead so the record is never a dead end.
    pub fn select_notification(&mut self, host_id: HostId, notification_id: NotificationId) {
        self.invalidate_github_provider_requests(&host_id);
        let linked_session = self.hosts.get(&host_id).and_then(|host| {
            let record = host.notifications.get(&notification_id.0)?;
            let session_id = record.session_id.as_ref()?;
            host.sessions
                .contains_key(&session_id.0)
                .then(|| session_id.clone())
        });
        self.selection = Some(match linked_session {
            Some(session_id) => Selection::Session {
                host_id,
                session_id,
            },
            None => Selection::Notification {
                host_id,
                notification_id,
            },
        });
    }

    /// Total unread notifications across all hosts.
    #[must_use]
    pub fn unread_notification_count(&self) -> usize {
        self.hosts.values().map(host_unread_count).sum()
    }

    /// Unread notifications for a single host.
    #[must_use]
    pub fn host_unread_notification_count(&self, host_id: &HostId) -> usize {
        self.hosts.get(host_id).map_or(0, host_unread_count)
    }

    /// Notifications matching `filter`, newest first.
    ///
    /// Ordering is by the stable `(created_at desc, id)` identity, never by the
    /// volatile lifecycle status, so marking a record read does not reshuffle
    /// rows under the operator's cursor.
    #[must_use]
    pub fn notifications(&self, filter: &NotificationFilter) -> Vec<NotificationRow> {
        let mut rows = Vec::new();
        for (host_id, host) in &self.hosts {
            if filter
                .host_id
                .as_ref()
                .is_some_and(|wanted| wanted != host_id)
            {
                continue;
            }
            for record in host.notifications.values() {
                if filter.matches(record) {
                    rows.push(NotificationRow {
                        host_id: host_id.clone(),
                        record: record.clone(),
                    });
                }
            }
        }
        rows.sort_by(|left, right| {
            right
                .record
                .created_at
                .cmp(&left.record.created_at)
                .then_with(|| left.record.id.0.cmp(&right.record.id.0))
        });
        rows
    }

    /// Look up one notification record by host and id.
    #[must_use]
    pub fn notification(
        &self,
        host_id: &HostId,
        id: &NotificationId,
    ) -> Option<&NotificationRecord> {
        self.hosts.get(host_id)?.notifications.get(&id.0)
    }

    /// The currently selected notification record, when a notification is
    /// selected and still present.
    #[must_use]
    pub fn selected_notification(&self) -> Option<&NotificationRecord> {
        match self.selection.as_ref()? {
            Selection::Notification {
                host_id,
                notification_id,
            } => self.notification(host_id, notification_id),
            _ => None,
        }
    }

    /// Build the agents monitor model from current host state.
    #[must_use]
    pub fn agent_monitor(&self) -> AgentMonitor {
        let mut monitor = AgentMonitor::default();
        for (host_id, host) in &self.hosts {
            for session in host.sessions.values() {
                match session.activity {
                    Some(AgentActivity::Blocked) => monitor.blocked += 1,
                    Some(AgentActivity::Working) => monitor.working += 1,
                    Some(AgentActivity::Idle) => monitor.idle += 1,
                    None => monitor.unknown += 1,
                }
                monitor.sessions.push(AgentRow {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                    name: session.name.clone(),
                    project_id: session.project_id.clone(),
                    project_label: session.project_label.clone(),
                    agent: session.agent.clone(),
                    activity: session.activity,
                    state: session.state.as_str().to_owned(),
                    branch: session.branch.clone(),
                });
            }
        }
        // Order by the stable (host, session) identity only — NEVER by the
        // volatile activity. Activity flips as agents work/block/idle on every
        // poll, and sorting on it would reshuffle rows under the operator's
        // cursor, making the list impossible to click. The activity is conveyed
        // by the per-row dot and the header counts instead.
        monitor.sessions.sort_by(|left, right| {
            left.host_id
                .cmp(&right.host_id)
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });
        monitor
    }
}

fn apply_provider_request_failure(
    host_id: &HostId,
    host: &mut HostView,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: Option<ProviderRequestId>,
) -> bool {
    let Some(request_id) = request_id else {
        return true;
    };
    match (provider, operation) {
        (SessionLinkProvider::Linear, ProviderOperation::LinearIssues) => {
            if host.provider.linear.active_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.linear.active_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubPullRequests) => {
            if host.provider.github.pull_requests_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.github.pull_requests_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubIssues) => {
            if host.provider.github.issues_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.github.issues_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubPullRequestStatus) => {
            if host.provider.github.pull_request_status_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.github.pull_request_status_request = None;
            true
        }
        (_, ProviderOperation::Launch) => true,
        _ => false,
    }
}

fn trace_ignored_unknown_host(host_id: &HostId, reason: &'static str) {
    tracing::event!(
        name: "gui.host.result.ignored",
        tracing::Level::DEBUG,
        host_id = %host_id,
        reason,
        "ignoring result for unknown host"
    );
}

fn trace_ignored_provider_result(
    host_id: &HostId,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: ProviderRequestId,
    reason: &'static str,
) {
    tracing::event!(
        name: "gui.provider.result.ignored",
        tracing::Level::DEBUG,
        host_id = %host_id,
        provider = ?provider,
        operation = ?operation,
        request_id = request_id.get(),
        reason,
        "ignoring provider result"
    );
}

fn trace_ignored_provider_failure(
    host_id: &HostId,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: ProviderRequestId,
    reason: &'static str,
) {
    tracing::event!(
        name: "gui.provider.failure.ignored",
        tracing::Level::DEBUG,
        host_id = %host_id,
        provider = ?provider,
        operation = ?operation,
        request_id = request_id.get(),
        reason,
        "ignoring provider failure"
    );
}

/// Derived row for the flat agents monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub host_id: HostId,
    pub session_id: SessionId,
    /// Owner-set display name, or `None` to show the row by its session id.
    pub name: Option<String>,
    pub project_id: Option<String>,
    pub project_label: Option<String>,
    pub agent: String,
    pub activity: Option<AgentActivity>,
    pub state: String,
    /// Branch checked out in the session's worktree, when bound.
    pub branch: Option<String>,
}

/// Derived agents monitor counts and rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMonitor {
    pub blocked: usize,
    pub working: usize,
    pub idle: usize,
    pub unknown: usize,
    pub sessions: Vec<AgentRow>,
}

/// One stable metadata row for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRow {
    pub key: String,
    pub value: String,
}

/// Return metadata rows in wire-stable key order.
#[must_use]
pub fn session_metadata_rows(session: &SessionInfo) -> Vec<MetadataRow> {
    session
        .metadata
        .iter()
        .map(|(key, value)| MetadataRow {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// Return parsed provider link metadata when a session is linked.
#[must_use]
pub fn session_link_metadata(session: &SessionInfo) -> Option<SessionLinkMetadata> {
    let provider =
        SessionLinkProvider::from_metadata(session.metadata.get("link.provider")?.as_str())?;
    let kind = SessionLinkKind::from_metadata(session.metadata.get("link.kind")?.as_str())?;
    let id = session.metadata.get("link.id")?.clone();
    let url = session.metadata.get("link.url")?.clone();
    let branch = session.metadata.get("link.branch")?.clone();
    SessionLinkMetadata::new(provider, kind, id, url, branch).ok()
}

fn apply_host_event(
    host: &mut HostView,
    host_id: &HostId,
    event: HostEvent,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    match event {
        HostEvent::AgentState(state) => {
            if let Some(session) = host.sessions.get_mut(&state.session_id.0) {
                session.activity = Some(state.activity);
                session.state_source = state.source;
            }
            host.last_agent_state = Some(state);
        }
        HostEvent::SessionCreated(session)
        | HostEvent::SessionUpdated(session)
        | HostEvent::SessionStopped(session) => {
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::SessionRemoved(session) => {
            host.sessions.remove(&session.id.0);
        }
        HostEvent::NotificationCreated(record) => {
            // A freshly created durable notification is the single source of OS
            // notifications: the daemon projector emits a durable `agent_blocked`
            // for a blocked session, which replaces the removed transient path.
            push_notification_effects(&record, host_id, notifications, toasts, next_intent_id);
            upsert_notification(&mut host.notifications, record);
        }
        HostEvent::NotificationUpdated(record) => {
            // Lifecycle/content changes (read, ack, archive, supersede) update the
            // stored record but never re-raise an OS intent.
            upsert_notification(&mut host.notifications, record);
        }
        HostEvent::NotificationDeleted(id) => {
            remove_notification(&mut host.notifications, &id);
        }
        HostEvent::Other(_) => {}
    }
}

/// Store or replace a notification record, dropping it when the daemon reports a
/// deleted lifecycle status so a hard-removed record cannot linger in the inbox.
fn upsert_notification(
    store: &mut BTreeMap<String, NotificationRecord>,
    record: NotificationRecord,
) {
    if record.status == NotificationStatus::Deleted {
        store.remove(&record.id.0);
    } else {
        store.insert(record.id.0.clone(), record);
    }
}

/// Remove a notification record by id.
fn remove_notification(store: &mut BTreeMap<String, NotificationRecord>, id: &NotificationId) {
    store.remove(&id.0);
}

/// Count unread notifications held by one host.
fn host_unread_count(host: &HostView) -> usize {
    host.notifications
        .values()
        .filter(|record| record.status == NotificationStatus::Unread)
        .count()
}

/// Whether a freshly created notification warrants a desktop OS notification.
///
/// Only durable action-required and error notifications interrupt the operator;
/// informational, success, and warning records land in the inbox silently.
fn notification_raises_intent(record: &NotificationRecord) -> bool {
    matches!(
        record.severity,
        NotificationSeverity::ActionRequired | NotificationSeverity::Error
    )
}

/// Append the OS intent (and, when session-linked, the in-app toast) for a newly
/// created notification that warrants an interrupt.
fn push_notification_effects(
    record: &NotificationRecord,
    host_id: &HostId,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    if !notification_raises_intent(record) {
        return;
    }
    let id = *next_intent_id;
    *next_intent_id += 1;
    notifications.push(NotificationIntent {
        id,
        host_id: host_id.clone(),
        notification_id: Some(record.id.clone()),
        session_id: record.session_id.clone(),
        title: record.title.clone(),
        body: record.body.clone(),
    });
    if let Some(session_id) = record.session_id.clone() {
        toasts.push(Toast {
            id,
            host_id: host_id.clone(),
            session_id,
            message: record.body.clone(),
        });
    }
}

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
    #[error("project action resolved provider `{actual}` but provider item requires `{expected}`")]
    ProviderActionMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("provider launch item is inconsistent: {message}")]
    ProviderLaunchItemMismatch { message: &'static str },
    #[error("provider `{provider}` cannot be converted to a prompt provider")]
    UnsupportedPromptProvider { provider: &'static str },
}

/// Load one host snapshot with `daemon.health` and `session.list`.
pub async fn load_host(config: HostConfig) -> Message {
    match load_host_snapshot(&config).await {
        Ok(snapshot) => Message::HostSnapshotLoaded { snapshot },
        Err(err) => Message::HostDisconnected {
            host_id: config.id,
            error: err.to_string(),
        },
    }
}

/// Load one host snapshot and return typed data for headless tests.
pub async fn load_host_snapshot(config: &HostConfig) -> Result<HostSnapshot, CoreError> {
    load_host_snapshot_with_options(config, ConnectionOptions::default()).await
}

/// Create a session on a host through the SDK.
pub async fn create_session(
    config: &HostConfig,
    params: SessionNewParams,
) -> Result<SessionNewResult, CoreError> {
    create_session_with_options(config, params, ConnectionOptions::default()).await
}

/// Create a session with explicit connection options.
pub async fn create_session_with_options(
    config: &HostConfig,
    params: SessionNewParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, CoreError> {
    call_host::<method::SessionNew>(config, options, params).await
}

/// Inspect a session on a host through the SDK.
pub async fn inspect_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionInfo, CoreError> {
    inspect_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Inspect a session with explicit connection options.
pub async fn inspect_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionInfo, CoreError> {
    call_host::<method::SessionInspect>(config, options, session_id.clone()).await
}

/// Resume a terminal session on a host through the SDK.
pub async fn resume_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionResumeResult, CoreError> {
    resume_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Resume a terminal session with explicit connection options.
pub async fn resume_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionResumeResult, CoreError> {
    call_host::<method::SessionResume>(config, options, session_id.clone()).await
}

/// Stop a session on a host through the SDK.
pub async fn stop_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionStopResult, CoreError> {
    stop_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Stop a session with explicit connection options.
pub async fn stop_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionStopResult, CoreError> {
    call_host::<method::SessionStop>(config, options, session_id.clone()).await
}

/// Remove a session from a host through the SDK.
///
/// Removal stops a still-live session first, then evicts it from the daemon's
/// registry so it stops appearing in `list`.
pub async fn remove_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionRemoveResult, CoreError> {
    remove_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Remove a session with explicit connection options.
pub async fn remove_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionRemoveResult, CoreError> {
    call_host::<method::SessionRemove>(config, options, session_id.clone()).await
}

/// Merge or clear session metadata on a host.
pub async fn set_session_metadata(
    config: &HostConfig,
    params: SessionSetMetadataParams,
) -> Result<SessionSetMetadataResult, CoreError> {
    set_session_metadata_with_options(config, params, ConnectionOptions::default()).await
}

/// Merge or clear metadata with explicit connection options.
pub async fn set_session_metadata_with_options(
    config: &HostConfig,
    params: SessionSetMetadataParams,
    options: ConnectionOptions,
) -> Result<SessionSetMetadataResult, CoreError> {
    call_host::<method::SessionSetMetadata>(config, options, params).await
}

/// Set or clear a session's display name on a host.
pub async fn rename_session(
    config: &HostConfig,
    params: SessionRenameParams,
) -> Result<SessionRenameResult, CoreError> {
    rename_session_with_options(config, params, ConnectionOptions::default()).await
}

/// Set or clear a session's display name with explicit connection options.
pub async fn rename_session_with_options(
    config: &HostConfig,
    params: SessionRenameParams,
    options: ConnectionOptions,
) -> Result<SessionRenameResult, CoreError> {
    call_host::<method::SessionRename>(config, options, params).await
}

/// List notification records on a host through the SDK.
pub async fn list_notifications(
    config: &HostConfig,
    params: NotificationListParams,
) -> Result<NotificationListResult, CoreError> {
    list_notifications_with_options(config, params, ConnectionOptions::default()).await
}

/// List notification records with explicit connection options.
pub async fn list_notifications_with_options(
    config: &HostConfig,
    params: NotificationListParams,
    options: ConnectionOptions,
) -> Result<NotificationListResult, CoreError> {
    call_host::<method::NotificationList>(config, options, params).await
}

/// Update a notification's lifecycle status on a host.
pub async fn update_notification(
    config: &HostConfig,
    params: NotificationUpdateParams,
) -> Result<NotificationUpdateResult, CoreError> {
    update_notification_with_options(config, params, ConnectionOptions::default()).await
}

/// Update a notification with explicit connection options.
pub async fn update_notification_with_options(
    config: &HostConfig,
    params: NotificationUpdateParams,
    options: ConnectionOptions,
) -> Result<NotificationUpdateResult, CoreError> {
    call_host::<method::NotificationUpdate>(config, options, params).await
}

/// Delete a notification record on a host.
pub async fn delete_notification(
    config: &HostConfig,
    params: NotificationDeleteParams,
) -> Result<NotificationDeleteResult, CoreError> {
    delete_notification_with_options(config, params, ConnectionOptions::default()).await
}

/// Delete a notification with explicit connection options.
pub async fn delete_notification_with_options(
    config: &HostConfig,
    params: NotificationDeleteParams,
    options: ConnectionOptions,
) -> Result<NotificationDeleteResult, CoreError> {
    call_host::<method::NotificationDelete>(config, options, params).await
}

/// List projects on a host through the SDK.
pub async fn list_projects(config: &HostConfig) -> Result<Vec<ProjectInfo>, CoreError> {
    list_projects_with_options(config, ConnectionOptions::default()).await
}

/// List projects with explicit connection options.
pub async fn list_projects_with_options(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<Vec<ProjectInfo>, CoreError> {
    call_host::<method::ProjectList>(
        config,
        options,
        ProjectListParams {
            filters: Vec::new(),
        },
    )
    .await
}

/// Add a project on a host through the SDK.
pub async fn add_project(
    config: &HostConfig,
    params: ProjectAddParams,
) -> Result<ProjectInfo, CoreError> {
    add_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Add a project with explicit connection options.
pub async fn add_project_with_options(
    config: &HostConfig,
    params: ProjectAddParams,
    options: ConnectionOptions,
) -> Result<ProjectInfo, CoreError> {
    call_host::<method::ProjectAdd>(config, options, params).await
}

/// Show a project and its live worktrees.
pub async fn show_project(
    config: &HostConfig,
    params: ProjectShowParams,
) -> Result<ProjectShowResult, CoreError> {
    show_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Show a project with explicit connection options.
pub async fn show_project_with_options(
    config: &HostConfig,
    params: ProjectShowParams,
    options: ConnectionOptions,
) -> Result<ProjectShowResult, CoreError> {
    call_host::<method::ProjectShow>(config, options, params).await
}

/// Rename a project on a host through the SDK.
pub async fn rename_project(
    config: &HostConfig,
    params: ProjectRenameParams,
) -> Result<ProjectInfo, CoreError> {
    rename_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Rename a project with explicit connection options.
pub async fn rename_project_with_options(
    config: &HostConfig,
    params: ProjectRenameParams,
    options: ConnectionOptions,
) -> Result<ProjectInfo, CoreError> {
    call_host::<method::ProjectRename>(config, options, params).await
}

/// Remove a project from a host.
pub async fn remove_project(
    config: &HostConfig,
    params: ProjectRemoveParams,
) -> Result<ProjectRemoveResult, CoreError> {
    remove_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Remove a project with explicit connection options.
pub async fn remove_project_with_options(
    config: &HostConfig,
    params: ProjectRemoveParams,
    options: ConnectionOptions,
) -> Result<ProjectRemoveResult, CoreError> {
    call_host::<method::ProjectRemove>(config, options, params).await
}

/// Remove a single pohunek-owned worktree from a host.
pub async fn remove_worktree(
    config: &HostConfig,
    params: WorktreeRemoveParams,
) -> Result<WorktreeRemoveResult, CoreError> {
    remove_worktree_with_options(config, params, ConnectionOptions::default()).await
}

/// Remove a single worktree with explicit connection options.
pub async fn remove_worktree_with_options(
    config: &HostConfig,
    params: WorktreeRemoveParams,
    options: ConnectionOptions,
) -> Result<WorktreeRemoveResult, CoreError> {
    call_host::<method::WorktreeRemove>(config, options, params).await
}

/// List project actions on a host through the SDK.
pub async fn list_project_actions(
    config: &HostConfig,
    params: ProjectActionsParams,
) -> Result<ProjectActionsResult, CoreError> {
    list_project_actions_with_options(config, params, ConnectionOptions::default()).await
}

/// List project actions with explicit connection options.
pub async fn list_project_actions_with_options(
    config: &HostConfig,
    params: ProjectActionsParams,
    options: ConnectionOptions,
) -> Result<ProjectActionsResult, CoreError> {
    call_host::<method::ProjectActions>(config, options, params).await
}

/// Resolve a project prompt on a host through the SDK.
pub async fn resolve_project_prompt(
    config: &HostConfig,
    params: ProjectPromptParams,
) -> Result<ProjectPromptResult, CoreError> {
    resolve_project_prompt_with_options(config, params, ConnectionOptions::default()).await
}

/// Resolve a project prompt with explicit connection options.
pub async fn resolve_project_prompt_with_options(
    config: &HostConfig,
    params: ProjectPromptParams,
    options: ConnectionOptions,
) -> Result<ProjectPromptResult, CoreError> {
    call_host::<method::ProjectPrompt>(config, options, params).await
}

/// Resolve a project action on a host through the SDK.
pub async fn resolve_project_action(
    config: &HostConfig,
    params: ProjectActionParams,
) -> Result<ProjectActionResult, CoreError> {
    resolve_project_action_with_options(config, params, ConnectionOptions::default()).await
}

/// Resolve a project action with explicit connection options.
pub async fn resolve_project_action_with_options(
    config: &HostConfig,
    params: ProjectActionParams,
    options: ConnectionOptions,
) -> Result<ProjectActionResult, CoreError> {
    call_host::<method::ProjectAction>(config, options, params).await
}

/// Render a resolved prompt template for preview.
pub fn preview_prompt_content(
    prompt_name: impl Into<String>,
    template_content: impl AsRef<str>,
    context: &PromptContext,
) -> Result<PromptPreview, CoreError> {
    let rendered = render_prompt(
        template_content.as_ref(),
        context.provider,
        context.item_id.as_str(),
        context.json.as_str(),
    )?;
    let branch = branch_from_context(context.provider, context.json.as_str())?;
    Ok(PromptPreview {
        prompt_name: prompt_name.into(),
        rendered,
        branch: Some(branch),
    })
}

/// Render a resolved project action prompt for preview.
pub fn preview_action_prompt(
    action: &ProjectActionResult,
    item_id: impl Into<String>,
    context_json: impl Into<String>,
) -> Result<PromptPreview, CoreError> {
    match &action.provider {
        ProviderKind::LinearIssue | ProviderKind::GithubPr => {
            let prompt_provider = action_prompt_provider(&action.provider)?;
            preview_prompt_content(
                action.prompt_name.clone(),
                &action.prompt_content,
                &PromptContext {
                    provider: prompt_provider,
                    item_id: item_id.into(),
                    json: context_json.into(),
                },
            )
        }
        ProviderKind::None => {
            let rendered = pohunek_prompt::render_static(&action.prompt_content)?;
            Ok(PromptPreview {
                prompt_name: action.prompt_name.clone(),
                rendered,
                branch: action.branch.clone(),
            })
        }
    }
}

/// Launch a rendered action prompt on a host.
pub async fn launch_action_prompt_with_options(
    config: &HostConfig,
    params: PromptLaunchParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, CoreError> {
    let branch = params
        .preview
        .branch
        .clone()
        .or_else(|| params.action.branch.clone());
    create_session_with_options(
        config,
        SessionNewParams {
            agent: params.action.agent,
            name: params.name,
            cwd: None,
            cols: params.cols,
            rows: params.rows,
            project: Some(params.project),
            repo: None,
            branch,
            base_branch: params.action.base_branch,
            input: Some(params.preview.rendered),
            metadata: params.metadata,
        },
        options,
    )
    .await
}

/// Resolve a provider action, render its prompt, and launch exactly one linked session.
pub async fn launch_provider_item_with_options(
    config: &HostConfig,
    params: ProviderLaunchParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, CoreError> {
    params.item.validate_link_invariants()?;
    let action = resolve_project_action_with_options(
        config,
        ProjectActionParams {
            reference: params.project.clone(),
            name: params.action_name,
        },
        options,
    )
    .await?;
    if action.provider != params.item.action_provider {
        return Err(CoreError::ProviderActionMismatch {
            expected: params.item.action_provider.as_str(),
            actual: action.provider.as_str(),
        });
    }

    let preview = preview_action_prompt(
        &action,
        params.item.item_id.clone(),
        params.item.context_json.clone(),
    )?;
    let branch = preview
        .branch
        .clone()
        .or_else(|| action.branch.clone())
        .ok_or(CoreError::MissingPromptBranch {
            provider: params.item.prompt_provider.as_str(),
        })?;
    let link = params.item.to_session_link(branch)?;
    launch_action_prompt_with_options(
        config,
        PromptLaunchParams {
            project: params.project,
            action,
            preview,
            cols: params.cols,
            rows: params.rows,
            metadata: link.to_session_metadata(),
            name: params.name,
        },
        options,
    )
    .await
}

fn action_prompt_provider(provider: &ProviderKind) -> Result<PromptProvider, CoreError> {
    match provider {
        ProviderKind::LinearIssue => Ok(PromptProvider::LinearIssue),
        ProviderKind::GithubPr => Ok(PromptProvider::GitHubPr),
        ProviderKind::None => Err(CoreError::UnsupportedPromptProvider {
            provider: provider.as_str(),
        }),
    }
}

fn branch_from_context(provider: PromptProvider, raw_json: &str) -> Result<String, CoreError> {
    let data: Value = serde_json::from_str(raw_json)?;
    let fields = match provider {
        PromptProvider::LinearIssue => providers::linear::ISSUE_BRANCH_FIELDS,
        PromptProvider::GitHubPr => providers::github::PULL_REQUEST_BRANCH_FIELDS,
    };
    fields
        .iter()
        .find_map(|field| {
            data.get(*field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .ok_or(CoreError::MissingPromptBranch {
            provider: provider.as_str(),
        })
}

async fn load_host_snapshot_with_options(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<HostSnapshot, CoreError> {
    let mut client = connect_client(config, options).await?;
    let health = HealthSummary::from(call_client::<method::DaemonHealth>(&mut client, ()).await?);
    let sessions = call_client::<method::SessionList>(
        &mut client,
        SessionListParams {
            filters: Vec::new(),
        },
    )
    .await?;
    let projects = match call_client::<method::ProjectList>(
        &mut client,
        ProjectListParams {
            filters: Vec::new(),
        },
    )
    .await
    {
        Ok(projects) => (projects, None),
        Err(err) => (Vec::new(), Some(format!("project.list failed: {err}"))),
    };
    let notifications = load_host_notifications(&mut client, &config.id).await;
    Ok(HostSnapshot {
        host_id: config.id.clone(),
        health,
        sessions,
        projects: projects.0,
        project_error: combine_seed_errors(projects.1, notifications.1),
        notifications: notifications.0,
    })
}

/// Seed recent notifications for one host, deduped across the seed queries.
///
/// Seeding is non-fatal: a host daemon without the notification surface answers
/// `method_not_found`, which is logged and treated as an empty inbox so the host
/// still connects. Runtime failures are surfaced through the snapshot's existing
/// degraded-status error channel. The daemon does not poison the connection on
/// a handled error, so this reuses the snapshot client after
/// `session.list`/`project.list`.
async fn load_host_notifications(
    client: &mut Client,
    host_id: &HostId,
) -> (Vec<NotificationRecord>, Option<String>) {
    let mut records: BTreeMap<String, NotificationRecord> = BTreeMap::new();
    let mut first_error = None;
    for params in notification_seed_queries() {
        match call_client::<method::NotificationList>(client, params).await {
            Ok(result) => {
                for record in result.notifications {
                    records.insert(record.id.0.clone(), record);
                }
            }
            Err(err) => {
                if notification_seed_unsupported(&err) {
                    tracing::event!(
                        name: "gui.notification_seed.unsupported",
                        tracing::Level::DEBUG,
                        host_id = %host_id,
                        error = %err,
                        "notification seed unsupported; treating as empty inbox"
                    );
                    return (Vec::new(), None);
                }
                tracing::event!(
                    name: "gui.notification_seed.query.failed",
                    tracing::Level::WARN,
                    host_id = %host_id,
                    error = %err,
                    "notification seed query failed; marking inbox degraded"
                );
                first_error.get_or_insert_with(|| format!("notification.list failed: {err}"));
            }
        }
    }
    (records.into_values().collect(), first_error)
}

fn combine_seed_errors(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

fn notification_seed_unsupported(err: &CoreError) -> bool {
    match err {
        CoreError::Client(
            pohunek_client::ClientError::Protocol(err)
            | pohunek_client::ClientError::RemoteProtocol { source: err, .. },
        )
        | CoreError::Protocol(err) => {
            err.class == protocol::ErrorClass::Daemon && err.code == METHOD_NOT_FOUND_CODE
        }
        _ => false,
    }
}

/// Seed queries run on connect and reconcile: recent unread first so an unread
/// backlog is never crowded out, then recent records of default live statuses
/// for read/archived context, then a bounded deleted tombstone window.
///
/// The tombstone query is intentionally limited to [`GUI_NOTIFICATION_SEED_LIMIT`]:
/// reconnect only reconciles deletes still covered by the daemon's recent
/// deleted window. Live delete events remain the authoritative path while the
/// GUI is connected, and seed reconciliation never raises OS intents.
fn notification_seed_queries() -> [NotificationListParams; 3] {
    [
        NotificationListParams {
            status: Some(NotificationStatus::Unread),
            limit: Some(GUI_NOTIFICATION_SEED_LIMIT),
            ..NotificationListParams::default()
        },
        NotificationListParams {
            limit: Some(GUI_NOTIFICATION_SEED_LIMIT),
            ..NotificationListParams::default()
        },
        NotificationListParams {
            status: Some(NotificationStatus::Deleted),
            limit: Some(GUI_NOTIFICATION_SEED_LIMIT),
            ..NotificationListParams::default()
        },
    ]
}

/// Each GUI command opens a short-lived client so reconnect state is localized
/// to the operation and does not share failure state with subscriptions.
async fn call_host<M>(
    config: &HostConfig,
    options: ConnectionOptions,
    params: M::Params,
) -> Result<M::Output, CoreError>
where
    M: protocol::Method,
{
    tracing::event!(
        name: "gui.host_request.client.open",
        tracing::Level::DEBUG,
        host_id = %config.id,
        method = M::NAME,
        "opening per-request GUI host client"
    );
    let mut client = match connect_client(config, options).await {
        Ok(client) => client,
        Err(err) => {
            tracing::event!(
                name: "gui.host_request.connect.failed",
                tracing::Level::WARN,
                host_id = %config.id,
                method = M::NAME,
                error = %err,
                "GUI host request connection failed"
            );
            return Err(err);
        }
    };
    match client.call::<M>(params).await {
        Ok(value) => {
            tracing::event!(
                name: "gui.host_request.completed",
                tracing::Level::DEBUG,
                host_id = %config.id,
                method = M::NAME,
                "GUI host request completed"
            );
            Ok(value)
        }
        Err(err) => {
            tracing::event!(
                name: "gui.host_request.failed",
                tracing::Level::WARN,
                host_id = %config.id,
                method = M::NAME,
                error = %err,
                "GUI host request failed"
            );
            Err(err.into())
        }
    }
}

async fn call_client<M>(client: &mut Client, params: M::Params) -> Result<M::Output, CoreError>
where
    M: protocol::Method,
{
    Ok(client.call::<M>(params).await?)
}

/// Build a reconnecting stream of messages for one host's event subscription.
pub fn host_subscription_stream(config: HostConfig) -> impl futures::Stream<Item = Message> {
    host_connection_stream(config, ConnectionOptions::default())
}

/// Build one reconnecting stream for every host and merge their messages.
pub fn workspace_connection_stream(
    hosts: Vec<HostConfig>,
    options: ConnectionOptions,
) -> impl futures::Stream<Item = Message> {
    stream::select_all(
        hosts
            .into_iter()
            .map(|host| host_connection_stream(host, options).boxed())
            .collect::<Vec<_>>(),
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "the reconnecting host worker is a single explicit async state machine"
)]
fn host_connection_stream(
    config: HostConfig,
    options: ConnectionOptions,
) -> impl futures::Stream<Item = Message> {
    stream::unfold(
        StreamState::Connecting {
            config,
            backoff: Backoff::new(options),
        },
        move |state| async move {
            match state {
                StreamState::Connecting { config, backoff } => {
                    let next = StreamState::Subscribing {
                        config: config.clone(),
                        backoff,
                    };
                    Some((
                        Message::HostConnecting {
                            host_id: config.id.clone(),
                        },
                        next,
                    ))
                }
                StreamState::Subscribing { config, backoff } => {
                    match subscribe_events(&config, options).await {
                        Ok(subscription) => Some((
                            Message::HostSubscribed {
                                host_id: config.id.clone(),
                            },
                            StreamState::LoadingSnapshot {
                                config,
                                subscription: Box::new(subscription),
                            },
                        )),
                        Err(err) => Some((
                            Message::HostUnreachable {
                                host_id: config.id.clone(),
                                error: err.to_string(),
                            },
                            StreamState::Waiting { config, backoff },
                        )),
                    }
                }
                StreamState::LoadingSnapshot {
                    config,
                    subscription,
                } => match load_host_snapshot_with_options(&config, options).await {
                    Ok(snapshot) => Some((
                        Message::HostSnapshotLoaded { snapshot },
                        StreamState::Reading {
                            config,
                            subscription,
                            interval: reconcile_interval(options.reconcile_interval),
                            backoff: Backoff::new(options),
                        },
                    )),
                    Err(err) => Some((
                        Message::HostDisconnected {
                            host_id: config.id.clone(),
                            error: err.to_string(),
                        },
                        StreamState::Waiting {
                            config,
                            backoff: Backoff::new(options),
                        },
                    )),
                },
                StreamState::Reading {
                    config,
                    mut subscription,
                    mut interval,
                    backoff,
                } => {
                    tokio::select! {
                        line = subscription.next_line() => {
                            match line {
                                Ok(Some(line)) => {
                                    let message = parse_event_message(&config.id, &line);
                                    Some((
                                        message.unwrap_or_else(|err| Message::HostDisconnected {
                                            host_id: config.id.clone(),
                                            error: err.to_string(),
                                        }),
                                        StreamState::Reading {
                                            config,
                                            subscription,
                                            interval,
                                            backoff,
                                        },
                                    ))
                                }
                                Ok(None) => Some((
                                    Message::HostDisconnected {
                                        host_id: config.id.clone(),
                                        error: "event subscription closed".to_owned(),
                                    },
                                    StreamState::Waiting { config, backoff },
                                )),
                                Err(err) => Some((
                                    Message::HostDisconnected {
                                        host_id: config.id.clone(),
                                        error: err.to_string(),
                                    },
                                    StreamState::Waiting { config, backoff },
                                )),
                            }
                        }
                        _ = interval.tick() => {
                            let message = match load_host_snapshot_with_options(&config, options).await {
                                Ok(snapshot) => Message::HostSnapshotLoaded { snapshot },
                                Err(err) => Message::HostDisconnected {
                                    host_id: config.id.clone(),
                                    error: err.to_string(),
                                },
                            };
                            Some((
                                message,
                                StreamState::Reading {
                                    config,
                                    subscription,
                                    interval,
                                    backoff,
                                },
                            ))
                        }
                    }
                }
                StreamState::Waiting {
                    config,
                    mut backoff,
                } => {
                    tokio::time::sleep(backoff.current).await;
                    backoff.advance();
                    Some((
                        Message::HostConnecting {
                            host_id: config.id.clone(),
                        },
                        StreamState::Subscribing { config, backoff },
                    ))
                }
            }
        },
    )
}

#[derive(Debug)]
enum StreamState {
    Connecting {
        config: HostConfig,
        backoff: Backoff,
    },
    Subscribing {
        config: HostConfig,
        backoff: Backoff,
    },
    LoadingSnapshot {
        config: HostConfig,
        subscription: Box<pohunek_client::Subscription>,
    },
    Reading {
        config: HostConfig,
        subscription: Box<pohunek_client::Subscription>,
        interval: tokio::time::Interval,
        backoff: Backoff,
    },
    Waiting {
        config: HostConfig,
        backoff: Backoff,
    },
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    current: Duration,
    max: Duration,
}

impl Backoff {
    fn new(options: ConnectionOptions) -> Self {
        let max = options.backoff_max.min(DEFAULT_BACKOFF_MAX);
        Self {
            current: options.backoff_initial.min(max),
            max,
        }
    }

    fn advance(&mut self) {
        self.current = self.current.saturating_mul(2).min(self.max);
    }
}

fn reconcile_interval(period: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(tokio::time::Instant::now() + period, period)
}

async fn subscribe_events(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<pohunek_client::Subscription, CoreError> {
    let client = connect_client(config, options).await?;
    let request = subscribe_request();
    Ok(client.subscribe(&request).await?)
}

fn subscribe_request() -> Request {
    Request::new(
        next_request_id(method::SUBSCRIBE),
        method::SUBSCRIBE,
        Value::Null,
    )
}

async fn connect_client(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<Client, CoreError> {
    let options = options.client();
    match &config.transport {
        HostTransport::Local { socket_path } => {
            Ok(Client::connect_local_with_options(socket_path, options).await?)
        }
        HostTransport::Remote { host, socket_path } => {
            Ok(Client::connect_with_options(host, socket_path, options).await?)
        }
        HostTransport::Tcp { addr } => {
            Ok(Client::connect_tcp_addr_with_options(config.id.as_str(), *addr, options).await?)
        }
    }
}

fn parse_event_message(host_id: &HostId, line: &str) -> Result<Message, CoreError> {
    let raw: Event = serde_json::from_str(line)?;
    let event = match raw.event.as_str() {
        event::AGENT_STATE => HostEvent::AgentState(parse_agent_state(raw)?),
        event::SESSION_CREATED => HostEvent::SessionCreated(parse_session_event(&raw)?),
        event::SESSION_UPDATED => HostEvent::SessionUpdated(parse_session_event(&raw)?),
        event::SESSION_STOPPED => HostEvent::SessionStopped(parse_session_event(&raw)?),
        event::SESSION_REMOVED => HostEvent::SessionRemoved(parse_session_event(&raw)?),
        event::NOTIFICATION_CREATED => {
            HostEvent::NotificationCreated(parse_notification_created(&raw)?)
        }
        event::NOTIFICATION_UPDATED => {
            HostEvent::NotificationUpdated(parse_notification_updated(&raw)?)
        }
        event::NOTIFICATION_DELETED => {
            HostEvent::NotificationDeleted(parse_notification_deleted(&raw)?)
        }
        _ => HostEvent::Other(raw),
    };
    Ok(Message::HostEvent {
        host_id: host_id.clone(),
        event,
    })
}

fn parse_notification_created(raw: &Event) -> Result<NotificationRecord, CoreError> {
    let event: NotificationCreatedEvent = serde_json::from_value(raw.payload.clone())?;
    Ok(event.record)
}

fn parse_notification_updated(raw: &Event) -> Result<NotificationRecord, CoreError> {
    let event: NotificationUpdatedEvent = serde_json::from_value(raw.payload.clone())?;
    Ok(event.record)
}

fn parse_notification_deleted(raw: &Event) -> Result<NotificationId, CoreError> {
    let event: NotificationDeletedEvent = serde_json::from_value(raw.payload.clone())?;
    Ok(event.notification_id)
}

fn parse_agent_state(raw: Event) -> Result<AgentStateEvent, CoreError> {
    let session_id = required_str(&raw.payload, "session_id")?;
    let activity = required_typed::<AgentActivity>(&raw.payload, "activity")?;
    let source = required_typed::<StateSource>(&raw.payload, "source")?;
    Ok(AgentStateEvent {
        session_id: SessionId(session_id.to_owned()),
        activity,
        source,
        raw,
    })
}

fn parse_session_event(raw: &Event) -> Result<SessionInfo, CoreError> {
    let session = raw
        .payload
        .get("session")
        .ok_or(CoreError::MissingSessionEventPayload)?;
    Ok(serde_json::from_value(session.clone())?)
}

fn required_str<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, CoreError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(CoreError::MissingAgentStateField { field })
}

/// Discover reachable remote hosts through the local daemon and include local.
pub async fn discover_hosts(
    local: HostConfig,
    options: ConnectionOptions,
) -> Result<Vec<HostConfig>, CoreError> {
    let mut client = connect_client(&local, options).await?;
    let records =
        call_client::<method::HostDiscover>(&mut client, HostDiscoverParams { force: false })
            .await?;
    let mut hosts = vec![local.clone()];
    for record in records {
        if matches!(record.class, HostClass::ReachableDaemon { .. }) {
            let host = discovered_transport_host(&record)?;
            let id = record
                .name
                .clone()
                .or_else(|| record.fqdn.clone())
                .unwrap_or_else(|| host.clone());
            let socket_path = match &local.transport {
                HostTransport::Local { socket_path }
                | HostTransport::Remote { socket_path, .. } => socket_path.clone(),
                HostTransport::Tcp { .. } => PathBuf::new(),
            };
            hosts.push(HostConfig::remote(id, host, socket_path));
        }
    }
    Ok(hosts)
}

fn discovered_transport_host(record: &HostRecord) -> Result<String, CoreError> {
    record
        .netbird_ip
        .clone()
        .or_else(|| record.fqdn.clone())
        .or_else(|| record.name.clone())
        .ok_or(CoreError::MissingDiscoveredHostName)
}

fn required_typed<T>(value: &Value, field: &'static str) -> Result<T, CoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value = value
        .get(field)
        .ok_or(CoreError::MissingAgentStateField { field })?;
    Ok(serde_json::from_value(value.clone())?)
}

/// Values filled into an attach command template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTemplateValues {
    pub bin: String,
    pub host: String,
    pub id: String,
}

/// Intent recorded after resolving and spawning attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSpawnIntent {
    pub command: String,
}

/// Port used by the shell to spawn external attach commands.
pub trait AttachCommandSpawner {
    /// Spawn `command` in the platform shell.
    fn spawn(&mut self, command: &str) -> Result<(), String>;
}

/// Render the configured attach command.
///
/// Replaces `{bin}`, `{host}`, and `{id}` with shell-escaped values because the
/// GUI shell spawner executes the rendered command through `sh -c`.
#[must_use]
pub fn render_attach_command(template: &str, values: &AttachTemplateValues) -> String {
    let bin = shell_escape(&values.bin);
    let host = shell_escape(&values.host);
    let id = shell_escape(&values.id);
    template
        .replace("{bin}", &bin)
        .replace("{host}", &host)
        .replace("{id}", &id)
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if value.bytes().all(is_shell_safe_byte) {
        return value.to_owned();
    }

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('\'');
    for character in value.chars() {
        if character == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(character);
        }
    }
    escaped.push('\'');
    escaped
}

const fn is_shell_safe_byte(byte: u8) -> bool {
    // POSIX shell metacharacters are intentionally excluded from this allowlist.
    matches!(
        byte,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'_'
            | b'-'
            | b'.'
            | b'/'
            | b':'
            | b'@'
            | b'%'
            | b'+'
            | b','
            | b'='
    )
}

/// Resolve and spawn an external attach command.
pub fn spawn_attach_command<S>(
    spawner: &mut S,
    template: &str,
    values: &AttachTemplateValues,
) -> Result<AttachSpawnIntent, String>
where
    S: AttachCommandSpawner + ?Sized,
{
    let command = render_attach_command(template, values);
    spawner.spawn(&command)?;
    Ok(AttachSpawnIntent { command })
}

#[cfg(test)]
mod tests {
    use protocol::NotificationSource;

    use super::*;

    #[test]
    fn workspace_applies_agent_state_to_known_session() {
        let mut workspace = Workspace::default();
        let session = session("s-1", None);
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session]),
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "blocked",
                "source": "osc_title"
            }),
        );
        workspace.apply(Message::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(
            host.sessions
                .get("s-1")
                .and_then(|session| session.activity),
            Some(AgentActivity::Blocked)
        );
        assert_eq!(
            host.last_agent_state
                .as_ref()
                .map(|event| event.session_id.0.as_str()),
            Some("s-1")
        );
    }

    #[test]
    fn workspace_accepts_report_agent_state_source() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None)]),
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "working",
                "source": "report"
            }),
        );
        workspace.apply(Message::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(
            host.last_agent_state.as_ref().map(|event| event.source),
            Some(StateSource::Report)
        );
        assert_eq!(
            host.sessions
                .get("s-1")
                .and_then(|session| session.activity),
            Some(AgentActivity::Working)
        );
    }

    #[test]
    fn session_remove_completed_drops_the_session() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None), session("s-2", None)]),
        });

        workspace.apply(Message::SessionRemoveCompleted {
            host_id: HostId::new("local"),
            session_id: SessionId("s-1".to_owned()),
            result: SessionRemoveResult {
                removed: true,
                stopped: true,
            },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(
            !host.sessions.contains_key("s-1"),
            "removed session is gone"
        );
        assert!(host.sessions.contains_key("s-2"), "other session remains");
    }

    #[test]
    fn agent_monitor_orders_by_identity_not_activity_and_carries_name() {
        let mut named = session("s-2", Some(AgentActivity::Working));
        named.name = Some("triage build".to_owned());
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![
                    session("s-3", Some(AgentActivity::Idle)),
                    named,
                    session("s-1", Some(AgentActivity::Blocked)),
                ],
            ),
        });

        let monitor = workspace.agent_monitor();
        let ids: Vec<&str> = monitor
            .sessions
            .iter()
            .map(|row| row.session_id.0.as_str())
            .collect();
        // Stable (host, id) order regardless of activity, so a row never jumps
        // out from under the cursor when its activity flips.
        assert_eq!(ids, ["s-1", "s-2", "s-3"]);
        assert_eq!(monitor.blocked, 1);
        assert_eq!(monitor.working, 1);
        assert_eq!(monitor.idle, 1);
        assert_eq!(monitor.sessions[1].name.as_deref(), Some("triage build"));
    }

    #[test]
    fn session_remove_completed_keeps_session_when_not_removed() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None)]),
        });

        // A `removed: false` result (a concurrent remove won the race) must not
        // touch the local view.
        workspace.apply(Message::SessionRemoveCompleted {
            host_id: HostId::new("local"),
            session_id: SessionId("s-1".to_owned()),
            result: SessionRemoveResult {
                removed: false,
                stopped: false,
            },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.sessions.contains_key("s-1"));
    }

    #[test]
    fn session_resumed_replaces_terminal_snapshot() {
        let mut stopped = session("s-1", None);
        stopped.state = SessionState::Stopped;
        let mut resumed = stopped.clone();
        resumed.state = SessionState::Running;
        resumed.pid = 99;

        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![stopped]),
        });

        workspace.apply(Message::SessionResumed {
            host_id: HostId::new("local"),
            result: SessionResumeResult { session: resumed },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        let session = host.sessions.get("s-1").expect("resumed session");
        assert_eq!(session.state, SessionState::Running);
        assert_eq!(session.pid, 99);
    }

    #[test]
    fn session_removed_event_drops_the_session() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None), session("s-2", None)]),
        });

        workspace.apply(Message::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::SessionRemoved(session("s-1", None)),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(
            !host.sessions.contains_key("s-1"),
            "removed session is gone"
        );
        assert!(host.sessions.contains_key("s-2"), "other session remains");
    }

    #[test]
    fn session_removed_wire_event_parses_to_host_event() {
        let raw = Event::new(
            event::SESSION_REMOVED,
            serde_json::json!({ "session": session("s-1", None) }),
        );
        let line = serde_json::to_string(&raw).expect("serialize event");

        let message =
            parse_event_message(&HostId::new("local"), &line).expect("parse session_removed");

        match message {
            Message::HostEvent {
                event: HostEvent::SessionRemoved(info),
                ..
            } => assert_eq!(info.id, SessionId("s-1".to_owned())),
            other => panic!("expected SessionRemoved host event, got {other:?}"),
        }
    }

    #[test]
    fn workspace_applies_provider_browser_state() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.apply(Message::ProviderPanelSelected {
            host_id: host_id.clone(),
            panel: ProviderPanel::GitHub,
        });
        workspace.apply(Message::LinearProviderFilterSelected {
            host_id: host_id.clone(),
            name: "Assigned to me".to_owned(),
        });
        workspace.apply(Message::GitHubProviderFilterSelected {
            host_id: host_id.clone(),
            name: "My PRs".to_owned(),
        });
        workspace.apply(Message::LinearProviderSearchChanged {
            host_id: host_id.clone(),
            value: "launcher".to_owned(),
        });
        let request_id = workspace.begin_linear_issues_request(host_id.clone());
        workspace.apply(Message::LinearProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id,
            filter_name: Some("Assigned to me".to_owned()),
            search: "launcher".to_owned(),
            issues: vec![providers::linear::LinearIssue {
                id: "opaque".to_owned(),
                identifier: "LIN-123".to_owned(),
                title: "Fix launcher".to_owned(),
                body: "Issue body".to_owned(),
                branch: "lin-123-fix-launcher".to_owned(),
                url: "https://linear.test/LIN-123".to_owned(),
            }],
        });
        workspace.apply(Message::LinearProviderIssueSelected {
            host_id: host_id.clone(),
            issue_id: "LIN-123".to_owned(),
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(host.provider.active_panel, ProviderPanel::GitHub);
        assert_eq!(host.provider.linear.search, "launcher");
        assert_eq!(
            host.provider.linear.selected_filter.as_deref(),
            Some("Assigned to me")
        );
        assert_eq!(
            host.provider.github.selected_filter.as_deref(),
            Some("My PRs")
        );
        assert_eq!(
            host.provider.linear.selected_issue_id.as_deref(),
            Some("LIN-123")
        );
    }

    #[test]
    fn workspace_records_github_pr_status_without_session_mutation() {
        let host_id = HostId::new("local");
        let scope = GitHubProviderScope::new("project-a", "/repo/a");
        let status_key =
            GitHubPullRequestStatusKey::new(scope, "https://github.example/repo/pull/7");
        let mut workspace = Workspace::default();
        let request_id = workspace.begin_github_pull_request_status_request(host_id.clone());
        workspace.apply(Message::GitHubProviderPullRequestStatusLoaded {
            host_id: host_id.clone(),
            request_id,
            status_key: status_key.clone(),
            status: providers::github::PullRequestStatus {
                review_decision: providers::github::ReviewDecision::Approved,
                checks: vec![providers::github::CheckRun {
                    name: "test".to_owned(),
                    status: "SUCCESS".to_owned(),
                    conclusion: Some("pass".to_owned()),
                    details_url: None,
                }],
            },
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.sessions.is_empty());
        assert!(host
            .provider
            .github
            .pull_request_statuses
            .contains_key(&status_key));
    }

    #[test]
    fn workspace_ignores_stale_linear_provider_results() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let request_id = workspace.begin_linear_issues_request(host_id.clone());
        workspace.apply(Message::LinearProviderSearchChanged {
            host_id: host_id.clone(),
            value: "new".to_owned(),
        });
        workspace.apply(Message::LinearProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id,
            filter_name: None,
            search: "old".to_owned(),
            issues: vec![providers::linear::LinearIssue {
                id: "opaque".to_owned(),
                identifier: "LIN-123".to_owned(),
                title: "Stale issue".to_owned(),
                body: String::new(),
                branch: "lin-123".to_owned(),
                url: "https://linear.test/LIN-123".to_owned(),
            }],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.linear.issues.is_empty());
    }

    #[test]
    fn github_provider_lists_are_scoped_to_project() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let pull_requests_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(Message::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: pull_requests_request,
            scope: GitHubProviderScope::new("project-a", "/repo/a"),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "A",
                "",
                "feature/a",
                "https://github.example/a/pull/7",
            )],
        });
        let issues_request = workspace.begin_github_issues_request(host_id.clone());
        workspace.apply(Message::GitHubProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id: issues_request,
            scope: GitHubProviderScope::new("project-b", "/repo/b"),
            issues: vec![providers::github::GitHubIssue {
                number: 7,
                title: "B".to_owned(),
                body: String::new(),
                url: "https://github.example/b/issues/7".to_owned(),
                branch: None,
            }],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(
            host.provider.github.scope.as_ref(),
            Some(&GitHubProviderScope::new("project-b", "/repo/b"))
        );
        assert!(host.provider.github.pull_requests.is_empty());
        assert_eq!(host.provider.github.issues.len(), 1);
    }

    #[test]
    fn github_provider_lists_are_scoped_to_repo_root() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let pull_requests_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(Message::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: pull_requests_request,
            scope: GitHubProviderScope::new("project-a", "/repo/a"),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "A",
                "",
                "feature/a",
                "https://github.example/a/pull/7",
            )],
        });
        let issues_request = workspace.begin_github_issues_request(host_id.clone());
        workspace.apply(Message::GitHubProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id: issues_request,
            scope: GitHubProviderScope::new("project-a", "/repo/b"),
            issues: vec![providers::github::GitHubIssue {
                number: 8,
                title: "B".to_owned(),
                body: String::new(),
                url: "https://github.example/b/issues/8".to_owned(),
                branch: None,
            }],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(
            host.provider.github.scope.as_ref(),
            Some(&GitHubProviderScope::new("project-a", "/repo/b"))
        );
        assert!(host.provider.github.pull_requests.is_empty());
        assert_eq!(host.provider.github.issues.len(), 1);
    }

    #[test]
    fn github_provider_ignores_stale_repo_scope_response() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: HostSnapshot {
                host_id: host_id.clone(),
                health: HealthSummary {
                    status: "ok".to_owned(),
                    daemon_version: "0.0.0".to_owned(),
                    protocol_version: protocol::PROTOCOL_VERSION,
                },
                sessions: Vec::new(),
                projects: vec![project("project-a", "/repo/current")],
                project_error: None,
                notifications: Vec::new(),
            },
        });
        workspace.selection = Some(Selection::Project {
            host_id: host_id.clone(),
            project_id: "project-a".to_owned(),
        });
        let request_id = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(Message::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id,
            scope: GitHubProviderScope::new("project-a", "/repo/stale"),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "Stale",
                "",
                "feature/stale",
                "https://github.example/stale/pull/7",
            )],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.github.scope.is_none());
        assert!(host.provider.github.pull_requests.is_empty());
    }

    #[test]
    fn github_provider_ignores_stale_same_scope_success() {
        let host_id = HostId::new("local");
        let scope = GitHubProviderScope::new("project-a", "/repo/current");
        let mut workspace = Workspace::default();
        let stale_request = workspace.begin_github_pull_requests_request(host_id.clone());
        let current_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(Message::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: current_request,
            scope: scope.clone(),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                9,
                "Current",
                "",
                "feature/current",
                "https://github.example/current/pull/9",
            )],
        });
        workspace.apply(Message::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: stale_request,
            scope,
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "Stale",
                "",
                "feature/stale",
                "https://github.example/stale/pull/7",
            )],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(host.provider.github.pull_requests.len(), 1);
        assert_eq!(host.provider.github.pull_requests[0].number, 9);
    }

    #[test]
    fn provider_operation_failed_ignores_stale_request() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let stale_request = workspace.begin_linear_issues_request(host_id.clone());
        let _current_request = workspace.begin_linear_issues_request(host_id.clone());
        workspace.apply(Message::ProviderOperationFailed {
            host_id: host_id.clone(),
            provider: SessionLinkProvider::Linear,
            operation: ProviderOperation::LinearIssues,
            request_id: Some(stale_request),
            error: "stale failure".to_owned(),
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.linear.last_error.is_none());
    }

    #[test]
    fn provider_operation_failed_ignores_request_invalidated_by_selection() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let stale_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.select_project(host_id.clone(), "project-a".to_owned());
        workspace.apply(Message::ProviderOperationFailed {
            host_id: host_id.clone(),
            provider: SessionLinkProvider::GitHub,
            operation: ProviderOperation::GitHubPullRequests,
            request_id: Some(stale_request),
            error: "stale failure".to_owned(),
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.github.last_error.is_none());
    }

    #[test]
    fn snapshot_seed_does_not_notify_existing_blocked_session() {
        let mut workspace = Workspace::default();
        let host_id = HostId::new("local");
        workspace.apply(Message::HostConnecting {
            host_id: host_id.clone(),
        });
        workspace.apply(Message::HostSubscribed {
            host_id: host_id.clone(),
        });

        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Blocked))]),
        });

        assert!(workspace.notification_intents.is_empty());
        assert!(workspace.toasts.is_empty());
    }

    #[test]
    fn blocked_session_agent_state_no_longer_emits_transient_intent() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Working))]),
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "blocked",
                "source": "osc_title"
            }),
        );
        workspace.apply(Message::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        // The transient blocked-session OS notification path was removed; OS
        // intents now originate from durable `notification_created` events that
        // the daemon projector produces for a blocked session.
        assert!(workspace.notification_intents.is_empty());
        assert!(workspace.toasts.is_empty());
    }

    #[test]
    fn host_snapshot_carries_notification_records() {
        let mut workspace = Workspace::default();
        let snapshot = snapshot_with_notifications(
            "local",
            vec![session("s-1", None)],
            vec![notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            )],
        );
        workspace.apply(Message::HostSnapshotLoaded { snapshot });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.notifications.contains_key("n-1"));
    }

    #[test]
    fn notification_created_event_parses_from_subscription_line() {
        let record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        let event = Event::new(
            event::NOTIFICATION_CREATED,
            serde_json::to_value(NotificationCreatedEvent {
                record: record.clone(),
            })
            .expect("payload"),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            Message::HostEvent {
                event: HostEvent::NotificationCreated(parsed),
                ..
            } => assert_eq!(parsed, record),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn notification_updated_event_parses_from_subscription_line() {
        let record = notification_record(
            "n-1",
            NotificationStatus::Read,
            NotificationSeverity::ActionRequired,
        );
        let event = Event::new(
            event::NOTIFICATION_UPDATED,
            serde_json::to_value(NotificationUpdatedEvent {
                record: record.clone(),
            })
            .expect("payload"),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            Message::HostEvent {
                event: HostEvent::NotificationUpdated(parsed),
                ..
            } => assert_eq!(parsed, record),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn notification_deleted_event_parses_from_subscription_line() {
        let event = Event::new(
            event::NOTIFICATION_DELETED,
            serde_json::to_value(NotificationDeletedEvent {
                notification_id: NotificationId("n-9".to_owned()),
            })
            .expect("payload"),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            Message::HostEvent {
                event: HostEvent::NotificationDeleted(id),
                ..
            } => assert_eq!(id, NotificationId("n-9".to_owned())),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn workspace_stores_notifications_per_host() {
        let mut workspace = Workspace::default();
        // Events only apply to hosts already known through the connect path; a
        // snapshot seeds each host before its notification events arrive.
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("host-a", vec![]),
        });
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("host-b", vec![]),
        });
        workspace.apply(notification_created(
            "host-a",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "host-b",
            notification_record(
                "n-2",
                NotificationStatus::Unread,
                NotificationSeverity::Error,
            ),
        ));

        let host_a = workspace.hosts.get(&HostId::new("host-a")).expect("host-a");
        let host_b = workspace.hosts.get(&HostId::new("host-b")).expect("host-b");
        assert!(host_a.notifications.contains_key("n-1"));
        assert!(!host_a.notifications.contains_key("n-2"));
        assert!(host_b.notifications.contains_key("n-2"));
    }

    #[test]
    fn unread_counts_track_lifecycle_transitions() {
        let mut workspace = Workspace::default();
        let host = HostId::new("local");
        // Seed the host through the connect path so its events are not dropped.
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-2",
                NotificationStatus::Unread,
                NotificationSeverity::Error,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 2);
        assert_eq!(workspace.host_unread_notification_count(&host), 2);

        workspace.apply(notification_updated(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Read,
                NotificationSeverity::ActionRequired,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 1);

        workspace.apply(notification_updated(
            "local",
            notification_record(
                "n-2",
                NotificationStatus::Acknowledged,
                NotificationSeverity::Error,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 0);

        workspace.apply(notification_updated(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Archived,
                NotificationSeverity::ActionRequired,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 0);

        workspace.apply(Message::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::NotificationDeleted(NotificationId("n-2".to_owned())),
        });
        assert!(!workspace
            .hosts
            .get(&host)
            .expect("host")
            .notifications
            .contains_key("n-2"));
    }

    #[test]
    fn reconnect_reconciliation_merges_missed_notifications_without_duplicates() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        let intents_after_live = workspace.notification_intents.len();

        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![],
                vec![
                    notification_record(
                        "n-1",
                        NotificationStatus::Unread,
                        NotificationSeverity::ActionRequired,
                    ),
                    notification_record(
                        "n-2",
                        NotificationStatus::Unread,
                        NotificationSeverity::ActionRequired,
                    ),
                ],
            ),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(host.notifications.len(), 2);
        assert!(host.notifications.contains_key("n-1"));
        assert!(host.notifications.contains_key("n-2"));
        // Seeding never emits OS intents; otherwise the periodic reconcile tick
        // would re-notify every still-unread record on every reload.
        assert_eq!(workspace.notification_intents.len(), intents_after_live);
    }

    #[test]
    fn reconnect_reconciliation_removes_seeded_deleted_notifications() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-deleted",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-live",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        let intents_after_live = workspace.notification_intents.len();

        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![],
                vec![
                    notification_record(
                        "n-live",
                        NotificationStatus::Unread,
                        NotificationSeverity::ActionRequired,
                    ),
                    notification_record(
                        "n-deleted",
                        NotificationStatus::Deleted,
                        NotificationSeverity::ActionRequired,
                    ),
                ],
            ),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.notifications.contains_key("n-live"));
        assert!(!host.notifications.contains_key("n-deleted"));
        assert_eq!(workspace.notification_intents.len(), intents_after_live);
    }

    #[test]
    fn notification_seed_queries_include_deleted_tombstone_window() {
        let queries = notification_seed_queries();

        assert!(
            queries
                .iter()
                .any(|params| params.status == Some(NotificationStatus::Deleted)),
            "reconnect seed must include deleted tombstones"
        );
    }

    #[test]
    fn selecting_linked_notification_selects_existing_session() {
        let mut workspace = Workspace::default();
        let mut record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        record.session_id = Some(SessionId("s-1".to_owned()));
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![session("s-1", Some(AgentActivity::Blocked))],
                vec![record],
            ),
        });

        workspace.select_notification(HostId::new("local"), NotificationId("n-1".to_owned()));

        assert_eq!(
            workspace.selection,
            Some(Selection::Session {
                host_id: HostId::new("local"),
                session_id: SessionId("s-1".to_owned()),
            })
        );
    }

    #[test]
    fn selecting_notification_without_live_session_opens_notification_detail() {
        let mut workspace = Workspace::default();
        let mut record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        record.session_id = Some(SessionId("s-gone".to_owned()));
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications("local", vec![], vec![record]),
        });

        workspace.select_notification(HostId::new("local"), NotificationId("n-1".to_owned()));

        assert_eq!(
            workspace.selection,
            Some(Selection::Notification {
                host_id: HostId::new("local"),
                notification_id: NotificationId("n-1".to_owned()),
            })
        );
        assert!(workspace.selected_notification().is_some());
    }

    #[test]
    fn action_required_notification_created_emits_single_os_intent() {
        let mut workspace = Workspace::default();
        // Seed the host through the connect path so its events are not dropped.
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        let mut record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        record.session_id = Some(SessionId("s-1".to_owned()));
        workspace.apply(notification_created("local", record));

        assert_eq!(workspace.notification_intents.len(), 1);
        let intent = &workspace.notification_intents[0];
        assert_eq!(
            intent.notification_id,
            Some(NotificationId("n-1".to_owned()))
        );
        assert_eq!(intent.session_id, Some(SessionId("s-1".to_owned())));
        // A session-linked notification also surfaces an in-app toast.
        assert_eq!(workspace.toasts.len(), 1);
    }

    #[test]
    fn informational_notification_created_does_not_emit_os_intent() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::Info,
            ),
        ));

        assert!(workspace.notification_intents.is_empty());
        assert!(workspace.toasts.is_empty());
    }

    #[test]
    fn snapshot_seed_does_not_emit_notification_intents() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![],
                vec![notification_record(
                    "n-1",
                    NotificationStatus::Unread,
                    NotificationSeverity::ActionRequired,
                )],
            ),
        });

        assert!(workspace.notification_intents.is_empty());
        assert!(workspace
            .hosts
            .get(&HostId::new("local"))
            .expect("host")
            .notifications
            .contains_key("n-1"));
    }

    #[test]
    fn notification_update_completed_message_updates_store() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(Message::NotificationUpdateCompleted {
            host_id: HostId::new("local"),
            result: NotificationUpdateResult {
                record: notification_record(
                    "n-1",
                    NotificationStatus::Read,
                    NotificationSeverity::ActionRequired,
                ),
            },
        });

        assert_eq!(workspace.unread_notification_count(), 0);
        assert_eq!(
            workspace
                .hosts
                .get(&HostId::new("local"))
                .expect("host")
                .notifications
                .get("n-1")
                .expect("n-1")
                .status,
            NotificationStatus::Read
        );
    }

    #[test]
    fn notification_delete_completed_message_removes_record() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(Message::NotificationDeleteCompleted {
            host_id: HostId::new("local"),
            result: NotificationDeleteResult {
                id: NotificationId("n-1".to_owned()),
                deleted: true,
            },
        });

        assert!(!workspace
            .hosts
            .get(&HostId::new("local"))
            .expect("host")
            .notifications
            .contains_key("n-1"));
    }

    #[test]
    fn notifications_selector_filters_and_orders_newest_first() {
        let mut workspace = Workspace::default();
        // Seed the host through the connect path so its events are not dropped.
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        let mut older = notification_record(
            "n-old",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        older.created_at = "2026-07-01T00:00:00Z".to_owned();
        let mut newer = notification_record(
            "n-new",
            NotificationStatus::Read,
            NotificationSeverity::Error,
        );
        newer.created_at = "2026-07-03T00:00:00Z".to_owned();
        newer.kind = NotificationKind::Error;
        newer.source.provider = "claude".to_owned();
        workspace.apply(notification_created("local", older));
        workspace.apply(notification_created("local", newer));

        let rows = workspace.notifications(&NotificationFilter::default());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].record.id, NotificationId("n-new".to_owned()));
        assert_eq!(rows[1].record.id, NotificationId("n-old".to_owned()));

        let unread = workspace.notifications(&NotificationFilter {
            status: Some(NotificationStatus::Unread),
            ..NotificationFilter::default()
        });
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].record.id, NotificationId("n-old".to_owned()));

        let errors = workspace.notifications(&NotificationFilter {
            severity: Some(NotificationSeverity::Error),
            ..NotificationFilter::default()
        });
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].record.id, NotificationId("n-new".to_owned()));

        let claude = workspace.notifications(&NotificationFilter {
            provider: Some("claude".to_owned()),
            ..NotificationFilter::default()
        });
        assert_eq!(claude.len(), 1);

        let error_kind = workspace.notifications(&NotificationFilter {
            kind: Some(NotificationKind::Error),
            ..NotificationFilter::default()
        });
        assert_eq!(error_kind.len(), 1);
    }

    #[test]
    fn notifications_selector_filters_by_host() {
        let mut workspace = Workspace::default();
        // Seed both hosts through the connect path so their events are not dropped.
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("host-a", vec![]),
        });
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("host-b", vec![]),
        });
        workspace.apply(notification_created(
            "host-a",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "host-b",
            notification_record(
                "n-2",
                NotificationStatus::Unread,
                NotificationSeverity::Error,
            ),
        ));

        let rows = workspace.notifications(&NotificationFilter {
            host_id: Some(HostId::new("host-b")),
            ..NotificationFilter::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host_id, HostId::new("host-b"));
        assert_eq!(rows[0].record.id, NotificationId("n-2".to_owned()));
    }

    #[test]
    fn discovered_transport_prefers_netbird_ip_over_short_name() {
        let record = HostRecord {
            name: Some("dev".to_owned()),
            fqdn: Some("dev.example.netbird.cloud".to_owned()),
            netbird_ip: Some("100.92.30.40".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.5.0".to_owned(),
            },
        };

        assert_eq!(
            discovered_transport_host(&record).expect("transport host"),
            "100.92.30.40"
        );
    }

    #[test]
    fn reconnect_backoff_is_capped_at_thirty_seconds() {
        let backoff = Backoff::new(ConnectionOptions {
            backoff_initial: Duration::from_secs(45),
            backoff_max: DEFAULT_BACKOFF_MAX.saturating_mul(4),
            ..ConnectionOptions::default()
        });

        assert_eq!(backoff.current, Duration::from_secs(30));
        assert_eq!(backoff.max, Duration::from_secs(30));
    }

    #[test]
    fn attach_command_replaces_declared_tokens() {
        let command = render_attach_command(
            "{bin} attach --host {host} {id}",
            &AttachTemplateValues {
                bin: "pohunek".to_owned(),
                host: "devbox".to_owned(),
                id: "s-7".to_owned(),
            },
        );

        assert_eq!(command, "pohunek attach --host devbox s-7");
    }

    #[test]
    fn attach_command_shell_escapes_substituted_tokens() {
        let command = render_attach_command(
            "{bin} attach --host {host} {id}",
            &AttachTemplateValues {
                bin: "/opt/pohunek bin".to_owned(),
                host: "devbox; touch /tmp/pwn".to_owned(),
                id: "s-7'$(touch /tmp/pwn)".to_owned(),
            },
        );

        assert_eq!(
            command,
            "'/opt/pohunek bin' attach --host 'devbox; touch /tmp/pwn' 's-7'\\''$(touch /tmp/pwn)'"
        );
    }

    #[test]
    fn provider_none_conversion_returns_error() {
        let err = action_prompt_provider(&ProviderKind::None).expect_err("provider none");

        assert!(matches!(
            err,
            CoreError::UnsupportedPromptProvider { provider: "none" }
        ));
    }

    #[test]
    fn subscribe_request_uses_sdk_request_id_generator() {
        let first = subscribe_request();
        let second = subscribe_request();

        assert_eq!(first.method, method::SUBSCRIBE);
        assert_eq!(first.params, Value::Null);
        assert!(first.id.starts_with("sdk-subscribe-"));
        assert!(second.id.starts_with("sdk-subscribe-"));
        assert_ne!(first.id, second.id);
    }

    fn snapshot(host_id: &str, sessions: Vec<SessionInfo>) -> HostSnapshot {
        HostSnapshot {
            host_id: HostId::new(host_id),
            health: HealthSummary {
                status: "ok".to_owned(),
                daemon_version: "0.0.0".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            },
            sessions,
            projects: Vec::new(),
            project_error: None,
            notifications: Vec::new(),
        }
    }

    fn session(id: &str, activity: Option<AgentActivity>) -> SessionInfo {
        SessionInfo {
            name: None,
            id: SessionId(id.to_owned()),
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/repo"),
            pid: 42,
            cols: 80,
            rows: 24,
            state: protocol::SessionState::Running,
            state_source: StateSource::Process,
            activity,
            native_session_id: None,
            native_session_path: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn project(id: &str, repo_root: &str) -> ProjectInfo {
        ProjectInfo {
            id: id.to_owned(),
            label: id.to_owned(),
            repo_root: PathBuf::from(repo_root),
            git_common_dir: PathBuf::from(repo_root).join(".git"),
            origin_url: None,
            default_base_branch: None,
            source: protocol::ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-01-01T00:00:00Z".to_owned(),
            last_used_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn snapshot_with_notifications(
        host_id: &str,
        sessions: Vec<SessionInfo>,
        notifications: Vec<NotificationRecord>,
    ) -> HostSnapshot {
        HostSnapshot {
            notifications,
            ..snapshot(host_id, sessions)
        }
    }

    fn notification_record(
        id: &str,
        status: NotificationStatus,
        severity: NotificationSeverity,
    ) -> NotificationRecord {
        NotificationRecord {
            id: NotificationId(id.to_owned()),
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "agent_blocked".to_owned(),
                host_local_source_id: format!("src-{id}"),
            },
            kind: NotificationKind::AgentBlocked,
            severity,
            status,
            title: format!("Notification {id}"),
            body: "Agent is waiting for input".to_owned(),
            metadata: BTreeMap::new(),
            created_at: "2026-07-03T00:00:00Z".to_owned(),
            session_id: None,
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        }
    }

    fn notification_created(host: &str, record: NotificationRecord) -> Message {
        Message::HostEvent {
            host_id: HostId::new(host),
            event: HostEvent::NotificationCreated(record),
        }
    }

    fn notification_updated(host: &str, record: NotificationRecord) -> Message {
        Message::HostEvent {
            host_id: HostId::new(host),
            event: HostEvent::NotificationUpdated(record),
        }
    }
}
