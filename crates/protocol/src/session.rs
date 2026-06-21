//! Typed session lifecycle payloads.
//!
//! The generic request, response, and event envelopes still carry opaque JSON
//! values. These shared types define the JSON shape both sides should use inside
//! those values for session lifecycle methods and events.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::envelope::StateSource;

/// The kind of agent backing a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// A plain shell session.
    Shell,
    /// A Codex CLI agent session.
    Codex,
    /// A Claude Code agent session.
    Claude,
}

/// Current detected agent activity within a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivity {
    /// The agent is actively processing work.
    Working,
    /// The agent is waiting for user input or approval.
    Blocked,
    /// The agent is running but not currently producing work.
    Idle,
}

/// Parameters for `session.new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNewParams {
    /// Agent kind to start.
    pub agent: AgentKind,
    /// Working directory for the session. If omitted, the daemon chooses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
    /// Git repository to bind a dedicated worktree for. When set together with
    /// `branch`, the daemon creates/binds one worktree per
    /// `(session, repository, branch)` and launches the agent inside it instead
    /// of in `cwd` (see `docs/plan-phase-1.md` "Worktree-per-Session"). `repo`
    /// and `branch` must be supplied together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    /// Branch to check out in the bound worktree. Requires `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Base branch the worktree's branch is created from. When the named base
    /// branch is missing the daemon falls back to the repository's default
    /// branch and records a non-fatal warning. Requires `repo` + `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

/// Opaque session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Parameters for `session.attach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachParams {
    /// Session to attach to.
    pub session_id: SessionId,
}

/// Result returned by `session.attach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachResult {
    /// One-shot stream identifier used by the attach connection header.
    pub stream_id: String,
}

/// Parameters for `session.input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInputParams {
    /// Session whose PTY should receive input.
    pub session_id: SessionId,
    /// Text to inject. The daemon applies agent-specific submit framing.
    pub text: String,
}

/// Result returned by `session.input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInputResult {
    /// Whether the daemon accepted the input for delivery.
    pub accepted: bool,
}

/// Parameters for `session.report_native_id`.
///
/// Fire-and-forget capture sent by an agent's `SessionStart` hook (see
/// `docs/plan-phase-1.md` "Hook Integration"). The hook learns the pohunek
/// `session_id` and `agent` from the launch-time handshake env and reads the
/// agent's own `native_session_id` (and optional `transcript_path`) from its
/// stdin JSON. The daemon records it as the session's resume binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReportNativeIdParams {
    /// The pohunek session id the agent was launched under.
    pub session_id: SessionId,
    /// Agent kind reporting its native session id.
    pub agent: AgentKind,
    /// The agent's own native session identifier used to build the resume argv.
    pub native_session_id: String,
    /// Optional transcript path reported by the agent (Claude provides one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// Result returned by `session.report_native_id`.
///
/// The hook fires-and-forgets and ignores this body; it exists so the method has
/// a typed, round-trippable response like every other control method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReportNativeIdResult {
    /// Whether the daemon recorded the report as a resume binding.
    pub recorded: bool,
}

/// Parameters for `session.detach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDetachParams {
    /// Active attach stream to detach.
    pub stream_id: String,
}

/// Result returned by `session.detach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDetachResult {
    /// Whether an active attach stream was detached.
    pub detached: bool,
}

/// Parameters for `session.resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResizeParams {
    /// Session whose PTY should be resized.
    pub session_id: SessionId,
    /// Updated terminal width in columns.
    pub cols: u16,
    /// Updated terminal height in rows.
    pub rows: u16,
}

/// Header sent as the first line on a raw attach connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachHeader {
    /// One-shot stream identifier returned by `session.attach`.
    pub attach: String,
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// The daemon accepted the session and is starting its process.
    Starting,
    /// The session process is running.
    Running,
    /// A stop was requested and the session is winding down.
    Stopped,
    /// The session completed successfully.
    Done,
    /// The session failed.
    Failed,
}

/// The kind of a non-fatal worktree-setup warning.
///
/// Each variant mirrors a Kandev worktree warning field (see
/// `docs/plan-phase-1.md` "Worktree-per-Session"): none of them aborts session
/// creation — the worktree is kept, the warning is surfaced, and the user
/// decides whether to intervene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionWarningKind {
    /// A `git fetch` from the configured remote failed; the worktree was bound
    /// from the local base ref instead, which may be out of date.
    Fetch,
    /// The requested base branch did not exist; the worktree was bound from the
    /// repository's default branch instead.
    BaseBranchFallback,
    /// The repository setup script failed; the worktree was kept without it.
    SetupScript,
}

/// A non-fatal warning surfaced while setting up a session's worktree.
///
/// Carries a machine-readable [`kind`](Self::kind), a human-readable summary,
/// and optional raw detail (e.g. trimmed git output) for debugging. Never
/// contains secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWarning {
    /// Machine-readable warning kind.
    pub kind: SessionWarningKind,
    /// Human-readable summary of what happened and what was done instead.
    pub message: String,
    /// Optional longer detail (e.g. trimmed git stderr) for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Summary returned by session lifecycle methods and published by events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Stable session identifier.
    pub id: SessionId,
    /// Agent kind backing the session.
    pub agent: AgentKind,
    /// Current working directory for the session.
    pub cwd: PathBuf,
    /// Operating-system process id of the session root process.
    pub pid: u32,
    /// Current terminal width in columns.
    pub cols: u16,
    /// Current terminal height in rows.
    pub rows: u16,
    /// Current lifecycle state.
    pub state: SessionState,
    /// Source of the current state signal.
    pub state_source: StateSource,
    /// Current detected agent activity, when the detector has published one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Native agent session id captured via the `SessionStart` hook, when one
    /// has been reported (see `docs/plan-phase-1.md` "Resume Model"). A session
    /// is resumable after a daemon restart only while this is present **and** the
    /// session is non-terminal: the daemon drops the resume binding on exit, so a
    /// terminal session can retain this id for reference yet not be resumable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Source git repository, when the session is bound to a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree, when the session has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Path to the bound worktree, when the session was launched in one. Equal
    /// to `cwd` for worktree sessions; absent for plain-`cwd` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Non-fatal warnings raised while setting up the worktree. Empty when the
    /// session has no worktree or setup was clean; omitted from the wire form
    /// when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<SessionWarning>,
    /// Creation timestamp in the daemon's wire timestamp format.
    pub created_at: String,
    /// Last update timestamp in the daemon's wire timestamp format.
    pub updated_at: String,
    /// Process exit code, when the session has exited with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Result returned by `session.stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStopResult {
    /// Whether the daemon stopped a live session.
    pub stopped: bool,
}

/// Result returned by `session.resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResizeResult {
    /// Updated session summary after the resize.
    pub session: SessionInfo,
}
