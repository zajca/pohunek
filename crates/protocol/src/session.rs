//! Typed session lifecycle payloads.
//!
//! The generic request, response, and event envelopes still carry opaque JSON
//! values. These shared types define the JSON shape both sides should use inside
//! those values for session lifecycle methods and events.

use std::{collections::BTreeMap, path::PathBuf};

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

impl AgentActivity {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
        }
    }
}

/// Parameters for `session.new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNewParams {
    /// Agent profile name to start.
    pub agent: String,
    /// Working directory for the session. If omitted, the daemon chooses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
    /// Project to run in, by `<id|label>` reference (see `docs/design/projects.md`).
    /// The daemon resolves it against **its own** project store and turns it into a
    /// host-local checkout, so no filesystem path crosses the wire — this is the
    /// only target option for a remote host. With `branch` it picks the worktree's
    /// source repository (the project's `repo_root`); without `branch` the agent
    /// runs in-place in that checkout. Takes precedence over `cwd` auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
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
    /// Initial text to inject into the freshly spawned PTY in the same
    /// `session.new` round-trip. The daemon applies the same agent-specific
    /// submit framing used by `session.input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Owner-controlled metadata for the session. Must not contain secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Parameters for `session.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionListParams {
    /// Exact-match filters applied with AND semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<SessionListFilter>,
}

/// A single exact-match session-list filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "key", content = "value", rename_all = "snake_case")]
pub enum SessionListFilter {
    /// Match [`SessionInfo::state`].
    State(SessionState),
    /// Match [`SessionInfo::activity`].
    Activity(AgentActivity),
    /// Match [`SessionInfo::agent`] or its snapshotted base kind label.
    Agent(String),
    /// Match [`SessionInfo::id`].
    Id(String),
    /// Match the session's project by `<id|label>` reference — its derived id
    /// ([`SessionInfo::project_id`]) or its enriched label
    /// ([`SessionInfo::project_label`]). Requires the list to be project-enriched
    /// (it is, by `session.list`).
    Project(String),
}

impl SessionListFilter {
    /// Whether this filter matches `session` exactly.
    #[must_use]
    pub fn matches(&self, session: &SessionInfo) -> bool {
        match self {
            Self::State(state) => session.state == *state,
            Self::Activity(activity) => session.activity == Some(*activity),
            Self::Agent(name) => {
                session.agent == *name || base_kind_label(session.agent_base) == name
            }
            Self::Id(id) => session.id.0 == *id,
            Self::Project(reference) => {
                session.project_id.as_deref() == Some(reference)
                    || session.project_label.as_deref() == Some(reference)
            }
        }
    }
}

fn base_kind_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
    }
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
    /// Session the attaching client is itself running inside, when known.
    ///
    /// Set by the CLI from `POHUNEK_SESSION_ID` (see
    /// [`ENV_SESSION_ID`](crate::ENV_SESSION_ID)): a process running inside a
    /// session's own PTY carries that session's id here. Paired with
    /// [`Self::origin_daemon_id`], it lets the daemon reject an attach that would
    /// pipe a PTY's output back into its own input (an infinite loop). Sent for
    /// every transport (the loop is reachable even over a same-host loopback TCP
    /// attach); the daemon-id pairing prevents a false positive against a
    /// different daemon that reuses the same id string. Additive: an older daemon
    /// ignores it; an older CLI omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<SessionId>,
    /// Daemon instance the [`Self::origin_session_id`] belongs to, from
    /// `POHUNEK_DAEMON_ID` (see [`ENV_DAEMON_ID`](crate::ENV_DAEMON_ID)).
    ///
    /// The daemon rejects the attach as self-feeding only when **both** the
    /// session id matches the target **and** this equals its own live instance id.
    /// That scopes the guard to the exact PTY the client sits inside: a colliding
    /// id on another daemon, or a stale value from a previous daemon process, has
    /// a different instance id and is correctly allowed. Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_daemon_id: Option<String>,
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
    /// Agent profile name reporting its native session id.
    pub agent: String,
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

impl SessionState {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Whether this is a **terminal** state — the session has stopped, completed,
    /// or failed and no longer holds resources (its worktree is free again, no
    /// process is attached). `Starting` and `Running` are the non-terminal,
    /// **live** states. Centralizing the set here keeps every "is this session
    /// still live?" check (daemon lifecycle guards, `project show` worktree
    /// enrichment) in agreement.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Done | Self::Failed)
    }
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
    /// A lifecycle hook (worktree pre/post-create or -remove, or a session-level
    /// hook) failed, timed out, or could not be run. Non-fatal: the session
    /// proceeds and the worktree is kept. The failing event name + reason ride in
    /// the warning's `message`/`detail`, not the kind (a **unit** variant, so the
    /// enum stays `Copy` and serializes to the bare string `"hook"`).
    Hook,
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
    /// Agent profile name backing the session.
    pub agent: String,
    /// Resolved base kind backing the session.
    pub agent_base: AgentKind,
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
    /// Native agent session **path** captured via the `SessionStart` hook, for an
    /// agent whose host profile resumes from a transcript path rather than an
    /// opaque id (`ref_kind = "path"`, Part C). Mutually exclusive with
    /// [`Self::native_session_id`]: a session resumes by exactly one of the two,
    /// chosen by its frozen `ref_kind`. `None` for the common id-resuming agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
    /// Project this session belongs to, by derived id (`p-…`), when it started
    /// inside (or was pointed at) a git repository. `None` for a session with no
    /// git identity (a plain shell in a non-git directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Current display label of [`Self::project_id`]'s project, **denormalized for
    /// display** and populated only by `session.list` (resolved fresh from the
    /// store at list time, so it reflects a rename). `None` for a session with no
    /// project or on responses that do not enrich it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_label: Option<String>,
    /// Whether the session's checkout is a linked git worktree rather than the
    /// repository's main checkout. `Some(true)` for a worktree-per-session, the
    /// detected value for an in-place session in a linked worktree, `Some(false)`
    /// for the main checkout, and `None` when the session has no git identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_linked_worktree: Option<bool>,
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
    /// Metadata set at creation or updated via `session.set_metadata`.
    /// Owner-controlled; must not contain secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// Creation timestamp in the daemon's wire timestamp format.
    pub created_at: String,
    /// Last update timestamp in the daemon's wire timestamp format.
    pub updated_at: String,
    /// Process exit code, when the session has exited with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Result returned by `session.new`.
///
/// The created session is flattened so the wire shape is a superset of
/// [`SessionInfo`]: a pre-`input` daemon (or any peer that does not understand
/// the field) still produces a plain `SessionInfo` object, and an older client
/// deserializing a newer daemon's reply simply ignores the extra
/// `applied_input` key — fully additive in both directions.
///
/// `applied_input` lets a client that sent [`SessionNewParams::input`] tell
/// whether the daemon actually injected it: a daemon that predates initial-input
/// support silently drops the field and returns `None` here, so the client can
/// warn instead of falsely reporting an injected prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNewResult {
    /// The freshly created session.
    #[serde(flatten)]
    pub session: SessionInfo,
    /// `Some(true)` when the daemon applied an initial `input` in this same
    /// round-trip; absent when no initial input was requested or the daemon does
    /// not support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_input: Option<bool>,
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

/// Parameters for `session.set_metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetMetadataParams {
    /// Session whose metadata should be merged.
    pub session_id: SessionId,
    /// Owner-controlled metadata patch. `Some(value)` sets a key and `None`
    /// removes it. Values must not contain secrets.
    pub metadata: BTreeMap<String, Option<String>>,
}

/// Result returned by `session.set_metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetMetadataResult {
    /// Updated session summary after the metadata merge.
    pub session: SessionInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `running`, `claude`, `working` session with the given id, for filter
    /// matching tests. This is the predicate the daemon actually runs
    /// (`handle_session_list`), so it is pinned directly here, not only through
    /// the daemon's socket/TCP integration tests.
    fn session(id: &str) -> SessionInfo {
        SessionInfo {
            id: SessionId(id.to_owned()),
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: PathBuf::from("/workspace"),
            pid: 4242,
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: Some(AgentActivity::Working),
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

    #[test]
    fn session_state_terminal_set_is_exactly_stopped_done_failed() {
        // The live states keep a worktree occupied; the terminal states free it.
        // `project show`'s worktree enrichment and the daemon lifecycle guards
        // both depend on this split, so pin it directly.
        assert!(!SessionState::Starting.is_terminal());
        assert!(!SessionState::Running.is_terminal());
        assert!(SessionState::Stopped.is_terminal());
        assert!(SessionState::Done.is_terminal());
        assert!(SessionState::Failed.is_terminal());
    }

    #[test]
    fn session_state_strings_match_wire_repr() {
        assert_eq!(SessionState::Starting.as_str(), "starting");
        assert_eq!(SessionState::Running.as_str(), "running");
        assert_eq!(SessionState::Stopped.as_str(), "stopped");
        assert_eq!(SessionState::Done.as_str(), "done");
        assert_eq!(SessionState::Failed.as_str(), "failed");
    }

    #[test]
    fn agent_activity_strings_match_wire_repr() {
        assert_eq!(AgentActivity::Working.as_str(), "working");
        assert_eq!(AgentActivity::Blocked.as_str(), "blocked");
        assert_eq!(AgentActivity::Idle.as_str(), "idle");
    }

    #[test]
    fn filter_matches_each_field() {
        let mut s = session("s-42");
        s.project_id = Some("p-abc".to_owned());
        s.project_label = Some("ui".to_owned());
        assert!(SessionListFilter::State(SessionState::Running).matches(&s));
        assert!(SessionListFilter::Agent("claude".to_owned()).matches(&s));
        assert!(SessionListFilter::Activity(AgentActivity::Working).matches(&s));
        assert!(SessionListFilter::Id("s-42".to_owned()).matches(&s));
        // A project filter matches the derived id or the enriched label.
        assert!(SessionListFilter::Project("p-abc".to_owned()).matches(&s));
        assert!(SessionListFilter::Project("ui".to_owned()).matches(&s));
    }

    #[test]
    fn filter_rejects_non_matching_field() {
        let s = session("s-42");
        assert!(!SessionListFilter::State(SessionState::Stopped).matches(&s));
        assert!(!SessionListFilter::Agent("codex".to_owned()).matches(&s));
        assert!(!SessionListFilter::Activity(AgentActivity::Blocked).matches(&s));
        // No project on this session ⇒ a project filter never matches.
        assert!(!SessionListFilter::Project("ui".to_owned()).matches(&s));
    }

    #[test]
    fn agent_filter_matches_profile_name_or_base_kind() {
        let mut s = session("s-42");
        s.agent = "claude-sonnet".to_owned();
        s.agent_base = AgentKind::Claude;

        assert!(SessionListFilter::Agent("claude-sonnet".to_owned()).matches(&s));
        assert!(SessionListFilter::Agent("claude".to_owned()).matches(&s));
        assert!(!SessionListFilter::Agent("codex".to_owned()).matches(&s));
    }

    #[test]
    fn id_filter_is_exact_not_prefix() {
        // Documented decision: id matching is exact, no prefix/glob in v1.
        let s = session("s-42");
        assert!(SessionListFilter::Id("s-42".to_owned()).matches(&s));
        assert!(!SessionListFilter::Id("s-4".to_owned()).matches(&s));
        assert!(!SessionListFilter::Id("s-420".to_owned()).matches(&s));
    }

    #[test]
    fn activity_filter_does_not_match_absent_activity() {
        let mut s = session("s-42");
        s.activity = None;
        assert!(!SessionListFilter::Activity(AgentActivity::Working).matches(&s));
        assert!(!SessionListFilter::Activity(AgentActivity::Idle).matches(&s));
    }

    #[test]
    fn attach_params_origin_is_additive_and_omitted_when_absent() {
        // Without an origin the wire form is exactly the pre-guard shape, so an
        // older daemon still parses it and a newer daemon sees `None` for both.
        let bare = SessionAttachParams {
            session_id: SessionId("s-1".to_owned()),
            origin_session_id: None,
            origin_daemon_id: None,
        };
        let value = serde_json::to_value(&bare).expect("serialize");
        assert_eq!(value, serde_json::json!({ "session_id": "s-1" }));
        // A pre-guard daemon's reply shape (no origin keys) still deserializes.
        let parsed: SessionAttachParams =
            serde_json::from_value(serde_json::json!({ "session_id": "s-1" })).expect("parse");
        assert_eq!(parsed, bare);
    }

    #[test]
    fn attach_params_origin_round_trips_when_present() {
        let with_origin = SessionAttachParams {
            session_id: SessionId("s-1".to_owned()),
            origin_session_id: Some(SessionId("s-1".to_owned())),
            origin_daemon_id: Some("daemon-abc".to_owned()),
        };
        let value = serde_json::to_value(&with_origin).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "session_id": "s-1",
                "origin_session_id": "s-1",
                "origin_daemon_id": "daemon-abc",
            })
        );
        let parsed: SessionAttachParams = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, with_origin);
    }
}
