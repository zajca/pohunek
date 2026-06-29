//! Headless state, SDK bridge logic, and command rendering for `pohunek-gui`.
//!
//! This crate intentionally has no Iced dependency. The native view layer wraps
//! these async helpers in Iced `Task` and `Subscription` values.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

pub mod providers;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures::{stream, StreamExt};
use pohunek_client::{Client, ClientOptions};
use protocol::{
    event, method, AgentActivity, Event, HostClass, HostDiscoverParams, HostRecord,
    ProjectActionParams, ProjectActionResult, ProjectActionsParams, ProjectActionsResult,
    ProjectAddParams, ProjectInfo, ProjectPromptParams, ProjectPromptResult, ProjectRemoveParams,
    ProjectRemoveResult, ProjectRenameParams, ProjectShowParams, ProjectShowResult,
    ProtocolVersion, ProviderKind, Request, SessionId, SessionInfo, SessionNewParams,
    SessionNewResult, SessionSetMetadataParams, SessionSetMetadataResult, SessionState,
    SessionStopResult, StateSource,
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
const DEFAULT_AGENTS_PANE_HEIGHT: u16 = 220;
const DEFAULT_WINDOW_WIDTH: u32 = 960;
const DEFAULT_WINDOW_HEIGHT: u32 = 640;

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

/// A host snapshot seeded by `daemon.health` and `session.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    pub host_id: HostId,
    pub health: HealthSummary,
    pub sessions: Vec<SessionInfo>,
    pub projects: Vec<ProjectInfo>,
    pub project_error: Option<String>,
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
    pub state_filter: String,
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
    SessionStopCompleted {
        host_id: HostId,
        session_id: SessionId,
        result: SessionStopResult,
    },
    SessionMetadataUpdated {
        host_id: HostId,
        result: SessionSetMetadataResult,
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
    LinearProviderStateFilterChanged {
        host_id: HostId,
        value: String,
    },
    LinearProviderSearchChanged {
        host_id: HostId,
        value: String,
    },
    LinearProviderIssuesLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        state_filter: String,
        search: String,
        issues: Vec<providers::linear::LinearIssue>,
    },
    LinearProviderIssueSelected {
        host_id: HostId,
        issue_id: String,
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
            Ok(raw) => Ok(toml::from_str(&raw).map_err(|source| UiStateError::Parse {
                path: path.clone(),
                source,
            })?),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationIntent {
    pub id: u64,
    pub host_id: HostId,
    pub session_id: SessionId,
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
                let previous_sessions = self.hosts.get(&host_id).map(|host| host.sessions.clone());
                if let Some(previous_sessions) = &previous_sessions {
                    for session in &snapshot.sessions {
                        if let Some(previous_session) = previous_sessions.get(&session.id.0) {
                            push_blocked_effects(
                                previous_session.activity,
                                session,
                                &host_id,
                                &mut self.notification_intents,
                                &mut self.toasts,
                                &mut self.next_intent_id,
                            );
                        }
                    }
                }
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
                let host = self
                    .hosts
                    .entry(host_id.clone())
                    .or_insert_with(HostView::connecting);
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
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Disconnected;
                host.last_error = Some(error);
            }
            Message::HostUnreachable { host_id, error } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Unreachable;
                host.last_error = Some(error);
            }
            Message::SessionCreated { host_id, session }
            | Message::SessionInspected { host_id, session } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
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
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                if let Some(session) = host.sessions.get_mut(&session_id.0) {
                    session.state = SessionState::Stopped;
                    session.activity = None;
                }
            }
            Message::SessionMetadataUpdated { host_id, result } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.sessions
                    .insert(result.session.id.0.clone(), result.session);
            }
            Message::ProjectListLoaded { host_id, projects } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
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
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.projects.insert(project.id.clone(), project);
            }
            Message::ProjectShown { host_id, result } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
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
            Message::ProjectActionsLoaded {
                host_id,
                reference,
                result,
            } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.prompt.actions_by_project.insert(reference, result);
                host.last_error = None;
            }
            Message::ProjectPromptResolved { host_id, prompt } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.prompt.resolved_prompt = Some(prompt);
                host.prompt.preview = None;
                host.last_error = None;
            }
            Message::ProjectActionResolved { host_id, action } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.prompt.resolved_action = Some(action);
                host.prompt.preview = None;
                host.last_error = None;
            }
            Message::PromptPreviewRendered { host_id, preview } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.prompt.preview = Some(preview);
                host.last_error = None;
            }
            Message::ProviderPanelSelected { host_id, panel } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.provider.active_panel = panel;
            }
            Message::LinearProviderStateFilterChanged { host_id, value } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.provider.linear.state_filter = value;
                host.provider.linear.active_request = None;
            }
            Message::LinearProviderSearchChanged { host_id, value } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.provider.linear.search = value;
                host.provider.linear.active_request = None;
            }
            Message::LinearProviderIssuesLoaded {
                host_id,
                request_id,
                state_filter,
                search,
                issues,
            } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                if host.provider.linear.active_request != Some(request_id) {
                    return;
                }
                if host.provider.linear.state_filter != state_filter
                    || host.provider.linear.search != search
                {
                    return;
                }
                host.provider.linear.active_request = None;
                host.provider.linear.issues = issues;
                host.provider.linear.selected_issue_id = None;
                host.provider.linear.last_error = None;
            }
            Message::LinearProviderIssueSelected { host_id, issue_id } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.provider.linear.selected_issue_id = Some(issue_id);
            }
            Message::GitHubProviderSearchChanged { host_id, value } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
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
                    return;
                }
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                if host.provider.github.pull_requests_request != Some(request_id) {
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
                    return;
                }
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                if host.provider.github.issues_request != Some(request_id) {
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
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.provider.github.selected_pull_request = Some(number);
            }
            Message::GitHubProviderIssueSelected { host_id, number } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
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
                    return;
                }
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                if host.provider.github.pull_request_status_request != Some(request_id) {
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
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                if !apply_provider_request_failure(host, provider, operation, request_id) {
                    return;
                }
                match provider {
                    SessionLinkProvider::Linear => host.provider.linear.last_error = Some(error),
                    SessionLinkProvider::GitHub => host.provider.github.last_error = Some(error),
                }
            }
            Message::HostOperationFailed { host_id, error } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.last_error = Some(error);
            }
        }
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
                    project_id: session.project_id.clone(),
                    project_label: session.project_label.clone(),
                    agent: session.agent.clone(),
                    activity: session.activity,
                    state: session.state.as_str().to_owned(),
                });
            }
        }
        monitor.sessions.sort_by(|left, right| {
            activity_rank(left.activity)
                .cmp(&activity_rank(right.activity))
                .then_with(|| left.host_id.cmp(&right.host_id))
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });
        monitor
    }
}

fn apply_provider_request_failure(
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
                return false;
            }
            host.provider.linear.active_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubPullRequests) => {
            if host.provider.github.pull_requests_request != Some(request_id) {
                return false;
            }
            host.provider.github.pull_requests_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubIssues) => {
            if host.provider.github.issues_request != Some(request_id) {
                return false;
            }
            host.provider.github.issues_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubPullRequestStatus) => {
            if host.provider.github.pull_request_status_request != Some(request_id) {
                return false;
            }
            host.provider.github.pull_request_status_request = None;
            true
        }
        (_, ProviderOperation::Launch) => true,
        _ => false,
    }
}

/// Derived row for the flat agents monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub host_id: HostId,
    pub session_id: SessionId,
    pub project_id: Option<String>,
    pub project_label: Option<String>,
    pub agent: String,
    pub activity: Option<AgentActivity>,
    pub state: String,
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

fn activity_rank(activity: Option<AgentActivity>) -> u8 {
    match activity {
        Some(AgentActivity::Blocked) => 0,
        Some(AgentActivity::Working) => 1,
        Some(AgentActivity::Idle) => 2,
        None => 3,
    }
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
                let previous = session.activity;
                session.activity = Some(state.activity);
                session.state_source = state.source;
                push_blocked_effects(
                    previous,
                    session,
                    host_id,
                    notifications,
                    toasts,
                    next_intent_id,
                );
            }
            host.last_agent_state = Some(state);
        }
        HostEvent::SessionCreated(session)
        | HostEvent::SessionUpdated(session)
        | HostEvent::SessionStopped(session) => {
            let previous = host
                .sessions
                .get(&session.id.0)
                .and_then(|existing| existing.activity);
            push_blocked_effects(
                previous,
                &session,
                host_id,
                notifications,
                toasts,
                next_intent_id,
            );
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::Other(_) => {}
    }
}

fn push_blocked_effects(
    previous: Option<AgentActivity>,
    session: &SessionInfo,
    host_id: &HostId,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    if previous == Some(AgentActivity::Blocked) || session.activity != Some(AgentActivity::Blocked)
    {
        return;
    }
    let id = *next_intent_id;
    *next_intent_id += 1;
    let title = "Agent blocked".to_owned();
    let body = format!(
        "{} on {} is waiting for input",
        session.agent,
        host_id.as_str()
    );
    notifications.push(NotificationIntent {
        id,
        host_id: host_id.clone(),
        session_id: session.id.clone(),
        title,
        body: body.clone(),
    });
    toasts.push(Toast {
        id,
        host_id: host_id.clone(),
        session_id: session.id.clone(),
        message: body,
    });
}

/// Errors raised by the GUI core bridge.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Client(#[from] pohunek_client::ClientError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Prompt(#[from] PromptError),
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
    request_host_json(
        config,
        options,
        "gui-session-new",
        method::SESSION_NEW,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-session-inspect",
        method::SESSION_INSPECT,
        serde_json::to_value(session_id)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-session-stop",
        method::SESSION_STOP,
        serde_json::to_value(session_id)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-session-set-metadata",
        method::SESSION_SET_METADATA,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-list",
        method::PROJECT_LIST,
        Value::Null,
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
    request_host_json(
        config,
        options,
        "gui-project-add",
        method::PROJECT_ADD,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-show",
        method::PROJECT_SHOW,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-rename",
        method::PROJECT_RENAME,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-remove",
        method::PROJECT_REMOVE,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-actions",
        method::PROJECT_ACTIONS,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-prompt",
        method::PROJECT_PROMPT,
        serde_json::to_value(params)?,
    )
    .await
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
    request_host_json(
        config,
        options,
        "gui-project-action",
        method::PROJECT_ACTION,
        serde_json::to_value(params)?,
    )
    .await
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
            let prompt_provider = action_prompt_provider(&action.provider);
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
        },
        options,
    )
    .await
}

fn action_prompt_provider(provider: &ProviderKind) -> PromptProvider {
    match provider {
        ProviderKind::LinearIssue => PromptProvider::LinearIssue,
        ProviderKind::GithubPr => PromptProvider::GitHubPr,
        ProviderKind::None => unreachable!("provider none is handled before provider conversion"),
    }
}

fn branch_from_context(provider: PromptProvider, raw_json: &str) -> Result<String, CoreError> {
    let data: Value = serde_json::from_str(raw_json)?;
    let fields = match provider {
        PromptProvider::LinearIssue => &["branchName", "branch"][..],
        PromptProvider::GitHubPr => &["headRefName", "branch", "branchName"][..],
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
    let health = request_json::<HealthSummary>(
        &mut client,
        "gui-daemon-health",
        method::DAEMON_HEALTH,
        Value::Null,
    )
    .await?;
    let sessions = request_json::<Vec<SessionInfo>>(
        &mut client,
        "gui-session-list",
        method::SESSION_LIST,
        Value::Null,
    )
    .await?;
    let projects = match request_json::<Vec<ProjectInfo>>(
        &mut client,
        "gui-project-list",
        method::PROJECT_LIST,
        Value::Null,
    )
    .await
    {
        Ok(projects) => (projects, None),
        Err(err) => (Vec::new(), Some(format!("project.list failed: {err}"))),
    };
    Ok(HostSnapshot {
        host_id: config.id.clone(),
        health,
        sessions,
        projects: projects.0,
        project_error: projects.1,
    })
}

async fn request_host_json<T>(
    config: &HostConfig,
    options: ConnectionOptions,
    id: &'static str,
    method: &'static str,
    params: Value,
) -> Result<T, CoreError>
where
    T: serde::de::DeserializeOwned,
{
    let mut client = connect_client(config, options).await?;
    request_json(&mut client, id, method, params).await
}

async fn request_json<T>(
    client: &mut Client,
    id: &'static str,
    method: &'static str,
    params: Value,
) -> Result<T, CoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value = client.request(&Request::new(id, method, params)).await?;
    Ok(serde_json::from_value(value)?)
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
    let request = Request::new("gui-subscribe", method::SUBSCRIBE, Value::Null);
    Ok(client.subscribe(&request).await?)
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
        _ => HostEvent::Other(raw),
    };
    Ok(Message::HostEvent {
        host_id: host_id.clone(),
        event,
    })
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
    let records = request_json::<Vec<HostRecord>>(
        &mut client,
        "gui-host-discover",
        method::HOST_DISCOVER,
        serde_json::to_value(HostDiscoverParams { force: false })?,
    )
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

/// Render the configured attach command by replacing `{bin}`, `{host}`, and `{id}`.
#[must_use]
pub fn render_attach_command(template: &str, values: &AttachTemplateValues) -> String {
    template
        .replace("{bin}", &values.bin)
        .replace("{host}", &values.host)
        .replace("{id}", &values.id)
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
    fn workspace_applies_provider_browser_state() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.apply(Message::ProviderPanelSelected {
            host_id: host_id.clone(),
            panel: ProviderPanel::GitHub,
        });
        workspace.apply(Message::LinearProviderSearchChanged {
            host_id: host_id.clone(),
            value: "launcher".to_owned(),
        });
        let request_id = workspace.begin_linear_issues_request(host_id.clone());
        workspace.apply(Message::LinearProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id,
            state_filter: String::new(),
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
            state_filter: String::new(),
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
            pull_requests: vec![providers::github::GitHubPullRequest {
                number: 7,
                title: "A".to_owned(),
                body: String::new(),
                head_ref_name: "feature/a".to_owned(),
                url: "https://github.example/a/pull/7".to_owned(),
            }],
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
            pull_requests: vec![providers::github::GitHubPullRequest {
                number: 7,
                title: "A".to_owned(),
                body: String::new(),
                head_ref_name: "feature/a".to_owned(),
                url: "https://github.example/a/pull/7".to_owned(),
            }],
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
            pull_requests: vec![providers::github::GitHubPullRequest {
                number: 7,
                title: "Stale".to_owned(),
                body: String::new(),
                head_ref_name: "feature/stale".to_owned(),
                url: "https://github.example/stale/pull/7".to_owned(),
            }],
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
            pull_requests: vec![providers::github::GitHubPullRequest {
                number: 9,
                title: "Current".to_owned(),
                body: String::new(),
                head_ref_name: "feature/current".to_owned(),
                url: "https://github.example/current/pull/9".to_owned(),
            }],
        });
        workspace.apply(Message::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: stale_request,
            scope,
            pull_requests: vec![providers::github::GitHubPullRequest {
                number: 7,
                title: "Stale".to_owned(),
                body: String::new(),
                head_ref_name: "feature/stale".to_owned(),
                url: "https://github.example/stale/pull/7".to_owned(),
            }],
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
    fn snapshot_relist_notifies_known_session_transition_to_blocked() {
        let mut workspace = Workspace::default();
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Working))]),
        });

        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Blocked))]),
        });

        assert_eq!(workspace.notification_intents.len(), 1);
        assert_eq!(workspace.notification_intents[0].session_id.0, "s-1");
        assert_eq!(workspace.toasts.len(), 1);
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
        }
    }

    fn session(id: &str, activity: Option<AgentActivity>) -> SessionInfo {
        SessionInfo {
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
}
