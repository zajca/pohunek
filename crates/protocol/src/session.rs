//! Typed session lifecycle payloads.
//!
//! The generic request, response, and event envelopes still carry opaque JSON
//! values. These shared types define the JSON shape both sides should use inside
//! those values for session lifecycle methods and events.

use std::{collections::BTreeMap, path::PathBuf};

use base64::prelude::{Engine as _, BASE64_STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    envelope::StateSource, ActivityRevision, OutputOffset, ProcessStartIdentity, ProtocolError,
    ReportSequence, RuntimeGeneration, TerminalWatermark, MAX_RUNTIME_ID_BYTES,
    MAX_SESSION_ID_BYTES, MAX_SESSION_OUTPUT_BYTES, MAX_SESSION_WAIT_MS,
};

/// The kind of agent backing a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentKind.ts"))]
#[cfg_attr(feature = "ts", ts(type = "string"))]
pub enum AgentKind {
    /// A plain shell session.
    Shell,
    /// A Codex CLI agent session.
    Codex,
    /// A Claude Code agent session.
    Claude,
    /// A Hermes Agent interactive terminal session.
    Hermes,
    /// A future wire value rendered neutrally by an older peer.
    Unknown(String),
}

impl AgentKind {
    /// Returns the stable wire value.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Shell => "shell",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Hermes => "hermes",
            Self::Unknown(value) => value,
        }
    }

    /// Returns whether this is a known, supported agent kind.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// Rejects presentation-only agent kinds before a mutation.
    ///
    /// # Errors
    ///
    /// Returns a stable `agent_kind_unsupported` protocol error for an unknown
    /// value. Call this from every agent-targeted public mutation.
    pub fn validate_mutation(&self) -> Result<(), ProtocolError> {
        if self.is_known() {
            Ok(())
        } else {
            Err(ProtocolError::agent_kind_unsupported(self.as_wire()))
        }
    }

    /// Rejects presentation-only agent kinds before durable persistence.
    ///
    /// # Errors
    ///
    /// Returns the same stable error as [`Self::validate_mutation`]. Keeping a
    /// dedicated entry point makes durable stores auditable without treating an
    /// unknown display value as a valid launch or recovery configuration.
    pub fn validate_persistence(&self) -> Result<(), ProtocolError> {
        self.validate_mutation()
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl Serialize for AgentKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for AgentKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "shell" => Self::Shell,
            "codex" => Self::Codex,
            "claude" => Self::Claude,
            "hermes" => Self::Hermes,
            other => Self::Unknown(other.to_owned()),
        })
    }
}

/// Current detected agent activity within a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentActivity.ts"))]
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

/// Source of the current working-directory value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "CwdSource.ts"))]
#[serde(rename_all = "snake_case")]
pub enum CwdSource {
    /// Captured at session launch or resume.
    Launch,
    /// Read from the focus process through procwatch.
    Procwatch,
    /// Reported by OSC 7 terminal output.
    Osc7,
}

impl CwdSource {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Procwatch => "procwatch",
            Self::Osc7 => "osc7",
        }
    }
}

/// Parameters for `session.new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionNewParams.ts"))]
pub struct SessionNewParams {
    /// Agent profile name to start.
    pub agent: String,
    /// Owner-set display name for the session. Cosmetic only: it never affects
    /// targeting or resume, and `None` leaves the session showing its id. The
    /// daemon trims it and rejects an over-long or control-character name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    /// Working directory for the session. If omitted, the daemon chooses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
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
    #[cfg_attr(feature = "ts", ts(optional))]
    pub project: Option<String>,
    /// Git repository to bind a dedicated worktree for. When set together with
    /// `branch`, the daemon creates/binds one worktree per
    /// `(session, repository, branch)` and launches the agent inside it instead
    /// of in `cwd` (see `docs/plan-phase-1.md` "Worktree-per-Session"). `repo`
    /// and `branch` must be supplied together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub repo: Option<PathBuf>,
    /// Branch to check out in the bound worktree. Requires `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub branch: Option<String>,
    /// Base branch the worktree's branch is created from. When the named base
    /// branch is missing the daemon falls back to the repository's default
    /// branch and records a non-fatal warning. Requires `repo` + `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub base_branch: Option<String>,
    /// Initial text to inject into the freshly spawned PTY in the same
    /// `session.new` round-trip. The daemon applies the same agent-specific
    /// submit framing used by `session.input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub input: Option<String>,
    /// Owner-controlled metadata for the session. Must not contain secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Working-directory policy for `session.fork`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ForkCwdMode.ts"))]
#[serde(rename_all = "snake_case")]
pub enum ForkCwdMode {
    /// Launch the fork in the source session's current worktree or directory.
    #[default]
    Same,
}

/// Parameters for `session.fork`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionForkParams.ts"))]
pub struct SessionForkParams {
    /// Source session whose native agent conversation should be forked.
    pub session_id: SessionId,
    /// Owner-set display name for the forked session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    /// Working-directory policy for the fork.
    #[serde(default)]
    pub cwd_mode: ForkCwdMode,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}

/// Parameters for `session.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionListParams.ts"))]
pub struct SessionListParams {
    /// Exact-match filters applied with AND semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<SessionListFilter>,
}

/// A single exact-match session-list filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionListFilter.ts"))]
#[serde(tag = "key", content = "value", rename_all = "snake_case")]
pub enum SessionListFilter {
    /// Match [`SessionInfo::state`].
    State(SessionState),
    /// Match [`SessionInfo::activity`].
    Activity(AgentActivity),
    /// Match launch or active agent identity by profile name or base kind label.
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
                session.agent == *name
                    || base_kind_label(&session.agent_base) == name
                    || session.active_agent.as_deref() == Some(name)
                    || session
                        .active_agent_base
                        .as_ref()
                        .is_some_and(|base| base_kind_label(base) == name)
            }
            Self::Id(id) => session.id.0 == *id,
            Self::Project(reference) => {
                session.project_id.as_deref() == Some(reference)
                    || session.project_label.as_deref() == Some(reference)
            }
        }
    }
}

fn base_kind_label(agent: &AgentKind) -> &str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
        AgentKind::Hermes => "hermes",
        AgentKind::Unknown(value) => value,
    }
}

/// Opaque session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionId.ts"))]
pub struct SessionId(pub String);

/// Validated terminal dimensions for an interactive attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "TerminalDimensions.ts"))]
pub struct TerminalDimensions {
    cols: u16,
    rows: u16,
}

impl TerminalDimensions {
    /// Creates nonzero terminal dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalDimensionsError::Zero`] when either axis is zero.
    pub const fn new(cols: u16, rows: u16) -> Result<Self, TerminalDimensionsError> {
        if cols == 0 || rows == 0 {
            Err(TerminalDimensionsError::Zero { cols, rows })
        } else {
            Ok(Self { cols, rows })
        }
    }

    /// Returns the terminal width in columns.
    #[must_use]
    pub const fn cols(self) -> u16 {
        self.cols
    }

    /// Returns the terminal height in rows.
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows
    }
}

impl<'de> Deserialize<'de> for TerminalDimensions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireTerminalDimensions {
            cols: u16,
            rows: u16,
        }

        let dimensions = WireTerminalDimensions::deserialize(deserializer)?;
        Self::new(dimensions.cols, dimensions.rows).map_err(serde::de::Error::custom)
    }
}

/// Reports invalid interactive terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TerminalDimensionsError {
    /// One or both terminal axes were zero.
    #[error("terminal dimensions must be nonzero, got {cols}x{rows}")]
    Zero {
        /// Requested terminal width in columns.
        cols: u16,
        /// Requested terminal height in rows.
        rows: u16,
    },
}

/// Parameters for `session.attach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionAttachParams.ts"))]
pub struct SessionAttachParams {
    /// Session to attach to.
    pub session_id: SessionId,
    /// Physical terminal geometry observed before attach negotiation, when known.
    ///
    /// The daemon binds these dimensions to the one-shot attach token so the
    /// worker can resize and capture the initial terminal snapshot before it
    /// emits any bytes on the raw stream. `None` represents a non-terminal or
    /// unknown client geometry; the worker retains its current geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub initial_dimensions: Option<TerminalDimensions>,
    /// Session the attaching client is itself running inside, when known.
    ///
    /// Set by the CLI from `POHUNEK_SESSION_ID` (see
    /// [`ENV_SESSION_ID`](crate::ENV_SESSION_ID)): a process running inside a
    /// session's own PTY carries that session's id here. Paired with
    /// [`Self::origin_worker_id`], it lets the daemon reject an attach that would
    /// pipe a PTY's output back into its own input (an infinite loop), including
    /// after daemon replacement. Sent for every transport because the loop is
    /// reachable even over a same-host loopback TCP attach. Additive: an older
    /// daemon ignores it; an older CLI omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub origin_session_id: Option<SessionId>,
    /// Daemon instance the [`Self::origin_session_id`] belongs to, from
    /// `POHUNEK_DAEMON_ID` (see [`ENV_DAEMON_ID`](crate::ENV_DAEMON_ID)).
    ///
    /// Worker-backed sessions ignore this value for self-feedback protection.
    /// It remains an additive compatibility fallback only for the isolated
    /// workerless test runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub origin_daemon_id: Option<String>,
    /// Stable worker identity inherited from the originating managed PTY.
    ///
    /// A daemon restart changes its instance ID but does not change the worker
    /// that owns the PTY. New peers use this field with
    /// [`Self::origin_session_id`] for the self-feedback guard. This stable pair
    /// is authoritative for production sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub origin_worker_id: Option<String>,
}

/// Result returned by `session.attach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionAttachResult.ts"))]
pub struct SessionAttachResult {
    /// One-shot stream identifier used by the attach connection header.
    pub stream_id: String,
}

/// Parameters for `session.input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionInputParams.ts"))]
pub struct SessionInputParams {
    /// Session whose PTY should receive input.
    pub session_id: SessionId,
    /// Text to inject. The daemon applies agent-specific submit framing.
    pub text: String,
    /// Optional bounded delivery-and-activity wait; absent keeps fire-and-forget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub wait: Option<SessionInputWait>,
}

/// Optional delivery-wait contract attached to a `session.input` request.
///
/// Agents that require delayed submit framing reject this contract before any
/// input bytes are written because activity cannot be revalidated inside the
/// worker-owned delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionInputWait.ts"))]
pub struct SessionInputWait {
    /// Agent activities that complete the wait once observed after submission.
    /// An absent or empty list defaults to `idle` and `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub until: Option<Vec<AgentActivity>>,
    /// Overall gate, worker-reservation, delivery, and activity-wait deadline in
    /// milliseconds from `1` to [`MAX_SESSION_WAIT_MS`]. A pre-send timeout
    /// writes no input; a post-send timeout may have an unknown delivery outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub timeout_ms: Option<u32>,
}

/// Result returned by `session.input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionInputResult.ts"))]
pub struct SessionInputResult {
    /// Whether the daemon accepted the input for delivery.
    pub accepted: bool,
    /// Final detected agent activity when a wait was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity: Option<AgentActivity>,
    /// Evidence source behind [`Self::activity`] when a wait was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity_source: Option<StateSource>,
    /// Runtime that produced the activity evidence for a waited delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime: Option<SessionRuntimeIdentity>,
    /// Daemon epoch that scopes [`Self::activity_revision`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity_epoch: Option<String>,
    /// Exact post-submission activity revision within [`Self::activity_epoch`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity_revision: Option<ActivityRevision>,
}

/// Reports an invalid bounded-observation request.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ObservationParamsError {
    /// A required identifier was empty or contained control characters.
    #[error("{field} must be nonempty and contain no control characters")]
    InvalidIdentifier {
        /// Invalid field name.
        field: &'static str,
    },
    /// An identifier exceeded its documented UTF-8 byte ceiling.
    #[error("{field} must be at most {maximum} UTF-8 bytes, got {actual}")]
    IdentifierTooLong {
        /// Invalid field name.
        field: &'static str,
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
    /// A cursor was supplied without the runtime that scopes it.
    #[error("{cursor} requires a runtime identity")]
    CursorRequiresRuntime {
        /// Cursor field name.
        cursor: &'static str,
    },
    /// A bounded byte count was zero or exceeded the protocol ceiling.
    #[error("max_bytes must be between 1 and {maximum}, got {actual}")]
    InvalidMaxBytes {
        /// Protocol maximum.
        maximum: u32,
        /// Requested value.
        actual: u32,
    },
    /// A required or optional wait duration was zero.
    #[error("{field} must be nonzero")]
    ZeroDuration {
        /// Invalid duration field.
        field: &'static str,
    },
    /// A bounded duration exceeded the public protocol ceiling.
    #[error("{field} must be at most {maximum} ms, got {actual}")]
    DurationTooLong {
        /// Invalid duration field.
        field: &'static str,
        /// Protocol maximum in milliseconds.
        maximum: u32,
        /// Requested duration in milliseconds.
        actual: u32,
    },
    /// Waiting output requires an explicit cursor.
    #[error("wait_ms requires after_offset")]
    OutputWaitRequiresCursor,
    /// A present state or activity predicate was empty.
    #[error("{field} must be omitted or contain at least one value")]
    EmptyPredicate {
        /// Empty predicate field.
        field: &'static str,
    },
    /// A wait contained no condition except its timeout.
    #[error("session.wait requires at least one predicate or cursor")]
    MissingPredicate,
    /// A timestamp was not valid RFC 3339.
    #[error("{field} must be a valid RFC 3339 timestamp")]
    InvalidTimestamp {
        /// Invalid timestamp field.
        field: &'static str,
    },
    /// A process identifier was zero.
    #[error("pid must be nonzero")]
    ZeroPid,
    /// Output data was not canonical standard base64.
    #[error("data_base64 must be canonical standard base64")]
    InvalidOutputBase64,
    /// Decoded output exceeded the protocol byte ceiling.
    #[error("decoded output must be at most {maximum} bytes, got {actual}")]
    OutputDataTooLarge {
        /// Protocol byte ceiling.
        maximum: usize,
        /// Actual decoded byte length.
        actual: usize,
    },
    /// Output offsets were not monotonically ordered.
    #[error("output offsets must satisfy history_start <= start <= next <= runtime_end")]
    InvalidOutputOffsetOrder,
    /// Decoded data length did not match the returned offset interval.
    #[error("decoded output length {decoded} does not match offset span {span}")]
    OutputLengthMismatch {
        /// Decoded data byte length.
        decoded: u64,
        /// `next_offset - start_offset`.
        span: u64,
    },
    /// Gap coordinates did not describe the evicted range ending at retained history.
    #[error("output gap must be nonempty and end at history_start_offset/start_offset")]
    InvalidOutputGap,
    /// `has_more` disagreed with the returned and runtime end offsets.
    #[error("has_more must equal next_offset < runtime_end_offset")]
    InvalidOutputHasMore,
}

fn validate_identifier(value: &str, field: &'static str) -> Result<(), ObservationParamsError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err(ObservationParamsError::InvalidIdentifier { field })
    } else {
        Ok(())
    }
}

fn validate_bounded_identifier(
    value: &str,
    field: &'static str,
    maximum: usize,
) -> Result<(), ObservationParamsError> {
    validate_identifier(value, field)?;
    if value.len() > maximum {
        Err(ObservationParamsError::IdentifierTooLong {
            field,
            maximum,
            actual: value.len(),
        })
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: &str, field: &'static str) -> Result<(), ObservationParamsError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|_timestamp| ())
        .map_err(|_error| ObservationParamsError::InvalidTimestamp { field })
}

/// Parameters for `session.screen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionScreenParams.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionScreenParams {
    /// Session whose managed terminal should be rendered.
    session_id: SessionId,
}

impl SessionScreenParams {
    /// Creates a terminal-screen request.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    /// Returns the requested logical session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Visible terminal cursor in a rendered screen snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "TerminalCursor.ts"))]
#[serde(deny_unknown_fields)]
pub struct TerminalCursor {
    /// Zero-based visible row.
    pub row: u16,
    /// Zero-based visible column.
    pub col: u16,
    /// Whether the terminal cursor is currently visible.
    pub visible: bool,
}

/// Runtime identity that scopes terminal cursors and output offsets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionRuntimeIdentity.ts"))]
pub struct SessionRuntimeIdentity {
    /// PTY runtime identifier.
    runtime_id: String,
    /// Monotonic logical-session generation for this runtime.
    runtime_generation: RuntimeGeneration,
}

impl<'de> Deserialize<'de> for SessionRuntimeIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireIdentity {
            runtime_id: String,
            runtime_generation: RuntimeGeneration,
        }
        let wire = WireIdentity::deserialize(deserializer)?;
        Self::new(wire.runtime_id, wire.runtime_generation).map_err(serde::de::Error::custom)
    }
}

impl SessionRuntimeIdentity {
    /// Creates a validated runtime identity.
    ///
    /// # Errors
    ///
    /// Returns [`ObservationParamsError::InvalidIdentifier`] for an empty or
    /// control-bearing runtime identifier.
    pub fn new(
        runtime_id: impl Into<String>,
        runtime_generation: RuntimeGeneration,
    ) -> Result<Self, ObservationParamsError> {
        let runtime_id = runtime_id.into();
        validate_bounded_identifier(&runtime_id, "runtime_id", MAX_RUNTIME_ID_BYTES)?;
        Ok(Self {
            runtime_id,
            runtime_generation,
        })
    }

    /// Returns the PTY runtime identifier.
    #[must_use]
    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    /// Returns the logical-session runtime generation.
    #[must_use]
    pub const fn runtime_generation(&self) -> RuntimeGeneration {
        self.runtime_generation
    }
}

/// Rendered terminal snapshot returned by `session.screen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionScreenResult.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionScreenResult {
    /// Logical session that owns the terminal.
    pub session_id: SessionId,
    /// Stable worker identity that supplied the snapshot.
    pub worker_id: String,
    /// Runtime identity scoped to this snapshot.
    #[serde(flatten)]
    pub runtime: SessionRuntimeIdentity,
    /// Monotonic terminal repaint revision.
    pub watermark: TerminalWatermark,
    /// Terminal geometry at the snapshot point.
    pub dimensions: TerminalDimensions,
    /// Cursor projection at the snapshot point.
    pub cursor: TerminalCursor,
    /// Whether the terminal is in its alternate screen buffer.
    pub alternate_screen: bool,
    /// Sanitized terminal title when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub title: Option<String>,
    /// Sanitized terminal progress when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub progress: Option<String>,
    /// Visible terminal lines with control sequences removed.
    pub visible_lines: Vec<String>,
}

/// Parameters for `session.output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionOutputParams.ts"))]
pub struct SessionOutputParams {
    /// Session whose retained PTY output should be read.
    session_id: SessionId,
    /// Runtime that owns [`Self::after_offset`], when continuing a read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    runtime: Option<SessionRuntimeIdentity>,
    /// Exclusive output cursor. Omission requests the newest retained tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    after_offset: Option<OutputOffset>,
    /// Maximum raw output bytes requested before base64 encoding.
    max_bytes: u32,
    /// Bounded wait used only when `after_offset` is at the current end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    wait_ms: Option<u32>,
}

impl SessionOutputParams {
    /// Creates a bounded output-read request.
    ///
    /// A cursor is meaningful only with its exact runtime identity. Waiting is
    /// allowed only at an explicit cursor, never on an implicit newest tail.
    ///
    /// # Errors
    ///
    /// Returns [`ObservationParamsError`] for an unscoped cursor, zero or
    /// over-limit byte count, zero wait, or wait without a cursor.
    pub fn new(
        session_id: SessionId,
        runtime: Option<SessionRuntimeIdentity>,
        after_offset: Option<OutputOffset>,
        max_bytes: u32,
        wait_ms: Option<u32>,
    ) -> Result<Self, ObservationParamsError> {
        if after_offset.is_some() && runtime.is_none() {
            return Err(ObservationParamsError::CursorRequiresRuntime {
                cursor: "after_offset",
            });
        }
        let maximum = u32::try_from(MAX_SESSION_OUTPUT_BYTES)
            .expect("session output ceiling is guaranteed to fit u32");
        if max_bytes == 0 || max_bytes > maximum {
            return Err(ObservationParamsError::InvalidMaxBytes {
                maximum,
                actual: max_bytes,
            });
        }
        if wait_ms == Some(0) {
            return Err(ObservationParamsError::ZeroDuration { field: "wait_ms" });
        }
        if wait_ms.is_some_and(|wait| wait > MAX_SESSION_WAIT_MS) {
            return Err(ObservationParamsError::DurationTooLong {
                field: "wait_ms",
                maximum: MAX_SESSION_WAIT_MS,
                actual: wait_ms.unwrap_or_default(),
            });
        }
        if wait_ms.is_some() && after_offset.is_none() {
            return Err(ObservationParamsError::OutputWaitRequiresCursor);
        }
        Ok(Self {
            session_id,
            runtime,
            after_offset,
            max_bytes,
            wait_ms,
        })
    }

    /// Returns the requested session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the runtime that scopes the cursor.
    #[must_use]
    pub const fn runtime(&self) -> Option<&SessionRuntimeIdentity> {
        self.runtime.as_ref()
    }

    /// Returns the exclusive output cursor.
    #[must_use]
    pub const fn after_offset(&self) -> Option<OutputOffset> {
        self.after_offset
    }

    /// Returns the requested raw-byte ceiling.
    #[must_use]
    pub const fn max_bytes(&self) -> u32 {
        self.max_bytes
    }

    /// Returns the optional bounded wait duration.
    #[must_use]
    pub const fn wait_ms(&self) -> Option<u32> {
        self.wait_ms
    }
}

impl<'de> Deserialize<'de> for SessionOutputParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireParams {
            session_id: SessionId,
            #[serde(default)]
            runtime: Option<SessionRuntimeIdentity>,
            #[serde(default)]
            after_offset: Option<OutputOffset>,
            max_bytes: u32,
            #[serde(default)]
            wait_ms: Option<u32>,
        }
        let wire = WireParams::deserialize(deserializer)?;
        Self::new(
            wire.session_id,
            wire.runtime,
            wire.after_offset,
            wire.max_bytes,
            wire.wait_ms,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Retained-history range missing before an output replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionOutputGap.ts"))]
pub struct SessionOutputGap {
    /// First unavailable byte offset requested by the caller.
    start_offset: OutputOffset,
    /// First retained byte offset returned by the daemon.
    end_offset: OutputOffset,
}

impl SessionOutputGap {
    /// Creates a nonempty evicted-output range.
    ///
    /// # Errors
    ///
    /// Returns [`ObservationParamsError::InvalidOutputGap`] unless `start_offset`
    /// is strictly before `end_offset`.
    pub fn new(
        start_offset: OutputOffset,
        end_offset: OutputOffset,
    ) -> Result<Self, ObservationParamsError> {
        if start_offset >= end_offset {
            Err(ObservationParamsError::InvalidOutputGap)
        } else {
            Ok(Self {
                start_offset,
                end_offset,
            })
        }
    }

    /// Returns the first unavailable requested offset.
    #[must_use]
    pub const fn start_offset(self) -> OutputOffset {
        self.start_offset
    }

    /// Returns the first retained offset after the gap.
    #[must_use]
    pub const fn end_offset(self) -> OutputOffset {
        self.end_offset
    }
}

impl<'de> Deserialize<'de> for SessionOutputGap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireGap {
            start_offset: OutputOffset,
            end_offset: OutputOffset,
        }

        let wire = WireGap::deserialize(deserializer)?;
        Self::new(wire.start_offset, wire.end_offset).map_err(serde::de::Error::custom)
    }
}

/// Bounded retained-output replay returned by `session.output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionOutputResult.ts"))]
pub struct SessionOutputResult {
    /// Logical session that owns the output.
    session_id: SessionId,
    /// Runtime identity that scopes every returned offset.
    #[serde(flatten)]
    runtime: SessionRuntimeIdentity,
    /// First retained output byte offset.
    history_start_offset: OutputOffset,
    /// First byte included in this response.
    start_offset: OutputOffset,
    /// Exclusive cursor for the next output request.
    next_offset: OutputOffset,
    /// Current exclusive end offset for the runtime.
    runtime_end_offset: OutputOffset,
    /// Raw output bytes encoded as standard base64.
    data_base64: String,
    /// Missing retained-history range, when the requested cursor was evicted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    gap: Option<SessionOutputGap>,
    /// Whether more bytes are immediately available after `next_offset`.
    has_more: bool,
    /// Whether a bounded wait elapsed without output.
    timed_out: bool,
}

impl SessionOutputResult {
    /// Creates a validated retained-output result.
    ///
    /// # Errors
    ///
    /// Returns [`ObservationParamsError`] when identifiers exceed their wire
    /// bounds, data is not canonical standard base64 or exceeds the protocol
    /// limit, offsets and decoded length disagree, gap coordinates are
    /// inconsistent, or `has_more` disagrees with the runtime end offset.
    #[expect(
        clippy::too_many_arguments,
        reason = "the arguments are the independent public output coordinates"
    )]
    pub fn new(
        session_id: SessionId,
        runtime: SessionRuntimeIdentity,
        history_start_offset: OutputOffset,
        start_offset: OutputOffset,
        next_offset: OutputOffset,
        runtime_end_offset: OutputOffset,
        data_base64: impl Into<String>,
        gap: Option<SessionOutputGap>,
        has_more: bool,
        timed_out: bool,
    ) -> Result<Self, ObservationParamsError> {
        validate_bounded_identifier(&session_id.0, "session_id", MAX_SESSION_ID_BYTES)?;
        let data_base64 = data_base64.into();
        let decoded = BASE64_STANDARD
            .decode(&data_base64)
            .map_err(|_error| ObservationParamsError::InvalidOutputBase64)?;
        if decoded.len() > MAX_SESSION_OUTPUT_BYTES {
            return Err(ObservationParamsError::OutputDataTooLarge {
                maximum: MAX_SESSION_OUTPUT_BYTES,
                actual: decoded.len(),
            });
        }

        let history_start = history_start_offset.get();
        let start = start_offset.get();
        let next = next_offset.get();
        let runtime_end = runtime_end_offset.get();
        if !(history_start <= start && start <= next && next <= runtime_end) {
            return Err(ObservationParamsError::InvalidOutputOffsetOrder);
        }
        let span = next - start;
        let decoded_len = u64::try_from(decoded.len()).expect("decoded output length fits u64");
        if decoded_len != span {
            return Err(ObservationParamsError::OutputLengthMismatch {
                decoded: decoded_len,
                span,
            });
        }
        if gap.is_some_and(|range| {
            range.end_offset() != history_start_offset || start_offset != history_start_offset
        }) {
            return Err(ObservationParamsError::InvalidOutputGap);
        }
        if has_more != (next < runtime_end) {
            return Err(ObservationParamsError::InvalidOutputHasMore);
        }

        Ok(Self {
            session_id,
            runtime,
            history_start_offset,
            start_offset,
            next_offset,
            runtime_end_offset,
            data_base64,
            gap,
            has_more,
            timed_out,
        })
    }

    /// Returns the logical session that owns the output.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the runtime identity that scopes the offsets.
    #[must_use]
    pub const fn runtime(&self) -> &SessionRuntimeIdentity {
        &self.runtime
    }

    /// Returns the first retained output offset.
    #[must_use]
    pub const fn history_start_offset(&self) -> OutputOffset {
        self.history_start_offset
    }

    /// Returns the first byte offset included in this response.
    #[must_use]
    pub const fn start_offset(&self) -> OutputOffset {
        self.start_offset
    }

    /// Returns the exclusive cursor for the next request.
    #[must_use]
    pub const fn next_offset(&self) -> OutputOffset {
        self.next_offset
    }

    /// Returns the current exclusive runtime end offset.
    #[must_use]
    pub const fn runtime_end_offset(&self) -> OutputOffset {
        self.runtime_end_offset
    }

    /// Returns the canonical standard-base64 output data.
    #[must_use]
    pub fn data_base64(&self) -> &str {
        &self.data_base64
    }

    /// Returns the missing retained-history range, when present.
    #[must_use]
    pub const fn gap(&self) -> Option<SessionOutputGap> {
        self.gap
    }

    /// Reports whether more output is immediately available.
    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.has_more
    }

    /// Reports whether a bounded output wait elapsed.
    #[must_use]
    pub const fn timed_out(&self) -> bool {
        self.timed_out
    }
}

impl<'de> Deserialize<'de> for SessionOutputResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireResult {
            session_id: SessionId,
            runtime_id: String,
            runtime_generation: RuntimeGeneration,
            history_start_offset: OutputOffset,
            start_offset: OutputOffset,
            next_offset: OutputOffset,
            runtime_end_offset: OutputOffset,
            data_base64: String,
            #[serde(default)]
            gap: Option<SessionOutputGap>,
            has_more: bool,
            timed_out: bool,
        }

        let wire = WireResult::deserialize(deserializer)?;
        let runtime = SessionRuntimeIdentity::new(wire.runtime_id, wire.runtime_generation)
            .map_err(serde::de::Error::custom)?;
        Self::new(
            wire.session_id,
            runtime,
            wire.history_start_offset,
            wire.start_offset,
            wire.next_offset,
            wire.runtime_end_offset,
            wire.data_base64,
            wire.gap,
            wire.has_more,
            wire.timed_out,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Parameters for the bounded long-poll `session.wait` method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWaitParams.ts"))]
pub struct SessionWaitParams {
    /// Session to observe.
    session_id: SessionId,
    /// Runtime identity whose replacement should wake the waiter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    runtime: Option<SessionRuntimeIdentity>,
    /// Session update cursor in the daemon's RFC 3339 wire format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    after_updated_at: Option<String>,
    /// Terminal watermark cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    after_terminal_watermark: Option<TerminalWatermark>,
    /// Output cursor scoped by [`Self::runtime`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    after_output_offset: Option<OutputOffset>,
    /// Terminal states that complete the wait when observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    states: Option<Vec<SessionState>>,
    /// Agent activities that complete the wait when observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    activities: Option<Vec<AgentActivity>>,
    /// Required nonzero bounded wait duration.
    timeout_ms: u32,
}

impl SessionWaitParams {
    /// Creates a validated bounded session wait.
    ///
    /// # Errors
    ///
    /// Returns [`ObservationParamsError`] when the timeout is zero, a present
    /// predicate is empty, a runtime-scoped cursor lacks a runtime identity, a
    /// timestamp is invalid, or no wake predicate is supplied.
    #[expect(
        clippy::too_many_arguments,
        reason = "the arguments are the independent public wait predicates"
    )]
    pub fn new(
        session_id: SessionId,
        runtime: Option<SessionRuntimeIdentity>,
        after_updated_at: Option<String>,
        after_terminal_watermark: Option<TerminalWatermark>,
        after_output_offset: Option<OutputOffset>,
        states: Option<Vec<SessionState>>,
        activities: Option<Vec<AgentActivity>>,
        timeout_ms: u32,
    ) -> Result<Self, ObservationParamsError> {
        if timeout_ms == 0 {
            return Err(ObservationParamsError::ZeroDuration {
                field: "timeout_ms",
            });
        }
        if timeout_ms > MAX_SESSION_WAIT_MS {
            return Err(ObservationParamsError::DurationTooLong {
                field: "timeout_ms",
                maximum: MAX_SESSION_WAIT_MS,
                actual: timeout_ms,
            });
        }
        if (after_terminal_watermark.is_some() || after_output_offset.is_some())
            && runtime.is_none()
        {
            return Err(ObservationParamsError::CursorRequiresRuntime {
                cursor: "terminal/output cursor",
            });
        }
        if states.as_ref().is_some_and(Vec::is_empty) {
            return Err(ObservationParamsError::EmptyPredicate { field: "states" });
        }
        if activities.as_ref().is_some_and(Vec::is_empty) {
            return Err(ObservationParamsError::EmptyPredicate {
                field: "activities",
            });
        }
        if let Some(timestamp) = &after_updated_at {
            validate_timestamp(timestamp, "after_updated_at")?;
        }
        if runtime.is_none()
            && after_updated_at.is_none()
            && after_terminal_watermark.is_none()
            && after_output_offset.is_none()
            && states.is_none()
            && activities.is_none()
        {
            return Err(ObservationParamsError::MissingPredicate);
        }
        Ok(Self {
            session_id,
            runtime,
            after_updated_at,
            after_terminal_watermark,
            after_output_offset,
            states,
            activities,
            timeout_ms,
        })
    }

    /// Returns the requested session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the runtime-change predicate and cursor scope.
    #[must_use]
    pub const fn runtime(&self) -> Option<&SessionRuntimeIdentity> {
        self.runtime.as_ref()
    }

    /// Returns the session update cursor.
    #[must_use]
    pub fn after_updated_at(&self) -> Option<&str> {
        self.after_updated_at.as_deref()
    }

    /// Returns the terminal watermark cursor.
    #[must_use]
    pub const fn after_terminal_watermark(&self) -> Option<TerminalWatermark> {
        self.after_terminal_watermark
    }

    /// Returns the output cursor.
    #[must_use]
    pub const fn after_output_offset(&self) -> Option<OutputOffset> {
        self.after_output_offset
    }

    /// Returns lifecycle-state predicates.
    #[must_use]
    pub fn states(&self) -> Option<&[SessionState]> {
        self.states.as_deref()
    }

    /// Returns agent-activity predicates.
    #[must_use]
    pub fn activities(&self) -> Option<&[AgentActivity]> {
        self.activities.as_deref()
    }

    /// Returns the bounded wait duration.
    #[must_use]
    pub const fn timeout_ms(&self) -> u32 {
        self.timeout_ms
    }
}

impl<'de> Deserialize<'de> for SessionWaitParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireParams {
            session_id: SessionId,
            #[serde(default)]
            runtime: Option<SessionRuntimeIdentity>,
            #[serde(default)]
            after_updated_at: Option<String>,
            #[serde(default)]
            after_terminal_watermark: Option<TerminalWatermark>,
            #[serde(default)]
            after_output_offset: Option<OutputOffset>,
            #[serde(default)]
            states: Option<Vec<SessionState>>,
            #[serde(default)]
            activities: Option<Vec<AgentActivity>>,
            timeout_ms: u32,
        }
        let wire = WireParams::deserialize(deserializer)?;
        Self::new(
            wire.session_id,
            wire.runtime,
            wire.after_updated_at,
            wire.after_terminal_watermark,
            wire.after_output_offset,
            wire.states,
            wire.activities,
            wire.timeout_ms,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Reason a bounded `session.wait` request completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWaitReason.ts"))]
#[serde(rename_all = "snake_case")]
pub enum SessionWaitReason {
    /// A requested lifecycle state is current.
    StateMatched,
    /// A requested agent activity is current.
    ActivityMatched,
    /// Metadata changed after the supplied session-update cursor.
    SessionUpdated,
    /// The terminal repaint watermark advanced.
    TerminalChanged,
    /// The runtime output cursor advanced.
    OutputAdvanced,
    /// The supplied runtime identity no longer matches.
    RuntimeChanged,
    /// The requested bounded wait elapsed.
    Timeout,
}

/// Result returned by the bounded long-poll `session.wait` method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWaitResult.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionWaitResult {
    /// Condition that completed the wait.
    pub reason: SessionWaitReason,
    /// Redacted current public session snapshot.
    pub session: SessionInfo,
    /// Current terminal watermark when a managed terminal is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub terminal_watermark: Option<TerminalWatermark>,
    /// Current output cursor when a managed terminal is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub output_offset: Option<OutputOffset>,
}

/// Parameters for `session.report_native_id`.
///
/// Fire-and-forget capture sent by an agent's `SessionStart` hook (see
/// `docs/plan-phase-1.md` "Hook Integration"). The hook learns the pohunek
/// `session_id` and `agent` from the launch-time handshake env and reads the
/// agent's own `native_session_id` (and optional `transcript_path`) from its
/// stdin JSON. The daemon records it as the session's resume binding.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "SessionReportNativeIdParams.ts")
)]
pub struct SessionReportNativeIdParams {
    /// The pohunek session id the agent was launched under.
    session_id: SessionId,
    /// Runtime identity that received the report.
    runtime_id: String,
    /// Agent profile name reporting its native session id.
    agent: String,
    /// Reporting process identifier.
    pid: u32,
    /// Kernel process-start identity paired with [`Self::pid`].
    pid_start_identity: ProcessStartIdentity,
    /// Strictly monotonic report sequence for this process/runtime claim.
    sequence: ReportSequence,
    /// RFC 3339 expiry after which the report is invalid.
    expires_at: String,
    /// The agent's own native session identifier used to build the resume argv.
    native_session_id: String,
    /// Optional transcript path reported by the agent (Claude provides one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    transcript_path: Option<String>,
}

impl SessionReportNativeIdParams {
    /// Creates a validated ordered native-identity report.
    ///
    /// # Errors
    ///
    /// Returns [`ObservationParamsError`] for invalid identifiers, zero PID, or
    /// a non-RFC-3339 expiry.
    #[expect(
        clippy::too_many_arguments,
        reason = "all arguments are authenticated identity-claim coordinates"
    )]
    pub fn new(
        session_id: SessionId,
        runtime_id: impl Into<String>,
        agent: impl Into<String>,
        pid: u32,
        pid_start_identity: ProcessStartIdentity,
        sequence: ReportSequence,
        expires_at: impl Into<String>,
        native_session_id: impl Into<String>,
        transcript_path: Option<String>,
    ) -> Result<Self, ObservationParamsError> {
        let runtime_id = runtime_id.into();
        let agent = agent.into();
        let expires_at = expires_at.into();
        let native_session_id = native_session_id.into();
        validate_bounded_identifier(&runtime_id, "runtime_id", MAX_RUNTIME_ID_BYTES)?;
        validate_identifier(&agent, "agent")?;
        validate_identifier(&native_session_id, "native_session_id")?;
        if pid == 0 {
            return Err(ObservationParamsError::ZeroPid);
        }
        validate_timestamp(&expires_at, "expires_at")?;
        Ok(Self {
            session_id,
            runtime_id,
            agent,
            pid,
            pid_start_identity,
            sequence,
            expires_at,
            native_session_id,
            transcript_path,
        })
    }

    /// Returns the logical session receiving the report.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the exact PTY runtime identifier.
    #[must_use]
    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    /// Returns the reporting agent profile.
    #[must_use]
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Returns the reporting process identifier.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns the paired kernel process-start identity.
    #[must_use]
    pub const fn pid_start_identity(&self) -> ProcessStartIdentity {
        self.pid_start_identity
    }

    /// Returns the monotonic report sequence.
    #[must_use]
    pub const fn sequence(&self) -> ReportSequence {
        self.sequence
    }

    /// Returns the validated RFC 3339 expiry.
    #[must_use]
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    /// Returns the provider-native recovery reference.
    #[must_use]
    pub fn native_session_id(&self) -> &str {
        &self.native_session_id
    }

    /// Returns the optional provider-native transcript path.
    #[must_use]
    pub fn transcript_path(&self) -> Option<&str> {
        self.transcript_path.as_deref()
    }
}

impl<'de> Deserialize<'de> for SessionReportNativeIdParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireParams {
            session_id: SessionId,
            runtime_id: String,
            agent: String,
            pid: u32,
            pid_start_identity: ProcessStartIdentity,
            sequence: ReportSequence,
            expires_at: String,
            native_session_id: String,
            #[serde(default)]
            transcript_path: Option<String>,
        }
        let wire = WireParams::deserialize(deserializer)?;
        Self::new(
            wire.session_id,
            wire.runtime_id,
            wire.agent,
            wire.pid,
            wire.pid_start_identity,
            wire.sequence,
            wire.expires_at,
            wire.native_session_id,
            wire.transcript_path,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Debug for SessionReportNativeIdParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionReportNativeIdParams")
            .field("session_id", &self.session_id)
            .field("runtime_id", &self.runtime_id)
            .field("agent", &self.agent)
            .field("pid", &self.pid)
            .field("pid_start_identity", &self.pid_start_identity)
            .field("sequence", &self.sequence)
            .field("expires_at", &self.expires_at)
            .field("native_session_id", &"[REDACTED]")
            .field(
                "transcript_path",
                &self.transcript_path.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Result returned by `session.report_native_id`.
///
/// The hook fires-and-forgets and ignores this body; it exists so the method has
/// a typed, round-trippable response like every other control method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "SessionReportNativeIdResult.ts")
)]
#[serde(deny_unknown_fields)]
pub struct SessionReportNativeIdResult {
    /// Whether the daemon recorded the report as a resume binding.
    pub recorded: bool,
}

/// Parameters for `session.report_agent`.
///
/// Fire-and-forget capture sent by an agent launched inside an existing
/// session. The daemon treats it as active runtime identity only; it does not
/// replace the parent session's launch agent or native resume binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionReportAgentParams.ts"))]
pub struct SessionReportAgentParams {
    /// The pohunek session id that currently hosts the nested agent.
    pub session_id: SessionId,
    /// Hook source identifier, usually `pohunek:<agent>`.
    pub source: String,
    /// Agent profile name reporting active ownership of the session.
    pub agent: String,
    /// Current activity reported by the hook, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity: Option<AgentActivity>,
    /// Optional monotonic sequence from the reporting hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub seq: Option<ReportSequence>,
    /// OS pid of the active nested agent process, when the hook can report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub pid: Option<u32>,
    /// Native session id for the active nested agent, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent_session_id: Option<String>,
    /// Native session path for the active nested agent, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent_session_path: Option<String>,
}

/// Parameters for `session.release_agent`.
///
/// Fire-and-forget release sent by a nested agent hook when the active agent no
/// longer owns the host session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionReleaseAgentParams.ts"))]
pub struct SessionReleaseAgentParams {
    /// The pohunek session id that currently hosts the nested agent.
    pub session_id: SessionId,
    /// Hook source identifier, usually `pohunek:<agent>`.
    pub source: String,
    /// Agent profile name releasing active ownership of the session.
    pub agent: String,
    /// Optional monotonic sequence from the reporting hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub seq: Option<ReportSequence>,
}

/// Result returned by `session.report_agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionReportAgentResult.ts"))]
pub struct SessionReportAgentResult {
    /// Whether the daemon recorded the active-agent report.
    pub recorded: bool,
}

/// Result returned by `session.release_agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionReleaseAgentResult.ts"))]
pub struct SessionReleaseAgentResult {
    /// Whether the daemon released the active-agent report.
    pub released: bool,
}

/// Parameters for `session.detach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionDetachParams.ts"))]
pub struct SessionDetachParams {
    /// Active attach stream to detach.
    pub stream_id: String,
}

/// Result returned by `session.detach`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionDetachResult.ts"))]
pub struct SessionDetachResult {
    /// Whether an active attach stream was detached.
    pub detached: bool,
    /// Stream-local failure recorded before the raw connection closed.
    ///
    /// Clients should surface this error instead of treating the closure as a
    /// transient daemon or runtime replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub error: Option<ProtocolError>,
}

/// Parameters for `session.resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionResizeParams.ts"))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AttachHeader.ts"))]
pub struct AttachHeader {
    /// One-shot stream identifier returned by `session.attach`.
    pub attach: String,
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionState.ts"))]
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

/// Availability of the PTY runtime backing a logical session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "RuntimeState.ts"))]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// A worker unit is being initialized.
    Starting,
    /// The existing worker and PTY are connected.
    Live,
    /// The daemon is reconnecting to a known worker.
    Reconnecting,
    /// The worker observed a terminal child outcome.
    Terminal,
    /// The worker or host was lost and the PTY no longer exists.
    Lost,
    /// More than one runtime identity claims the logical session.
    Conflict,
    /// The live worker has no compatible private protocol version.
    Incompatible,
}

/// Discovery classification for one independently surviving worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "RuntimeInventoryStatus.ts"))]
#[serde(rename_all = "snake_case")]
pub enum RuntimeInventoryStatus {
    /// The worker is the unique authenticated runtime for its logical session.
    Managed,
    /// The worker has no matching logical session and is deliberately left alive.
    Orphaned,
    /// Multiple authenticated workers claim the same logical session.
    Conflict,
    /// The worker speaks no compatible private protocol version.
    Incompatible,
    /// The runtime-directory, logical-record, and worker identities disagree.
    IdentityMismatch,
}

/// Public, read-only inventory entry for one discovered worker endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "RuntimeInventoryEntry.ts"))]
pub struct RuntimeInventoryEntry {
    /// Owner-private runtime-directory name containing the worker socket.
    pub runtime_slot: String,
    /// Session identity authenticated through the worker protocol, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub claimed_session_id: Option<String>,
    /// Stable worker identity, when negotiation and inspection succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub worker_id: Option<String>,
    /// Current PTY generation identity, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime_id: Option<String>,
    /// Fail-closed discovery classification.
    pub status: RuntimeInventoryStatus,
    /// Stable machine-readable explanation for non-managed entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<String>,
}

/// Result of `session.runtime_inventory`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "RuntimeInventoryResult.ts"))]
pub struct RuntimeInventoryResult {
    /// Authenticated endpoints and quarantined discovery failures.
    pub entries: Vec<RuntimeInventoryEntry>,
}

/// Event payload emitted when discovery quarantines a worker endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "RuntimeInventoryEvent.ts"))]
pub struct RuntimeInventoryEvent {
    /// Newly classified worker endpoint.
    pub entry: RuntimeInventoryEntry,
}

/// Runtime generation attached to a logical session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionRuntime.ts"))]
pub struct SessionRuntime {
    /// Current runtime availability.
    pub state: RuntimeState,
    /// Monotonic generation scoped to the logical session.
    pub runtime_generation: RuntimeGeneration,
    /// Stable owner of the PTY, when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub worker_id: Option<String>,
    /// PTY generation identity, when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime_id: Option<String>,
    /// Timestamp at which this runtime generation started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub started_at: Option<String>,
    /// Timestamp of the latest successful daemon connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub last_connected_at: Option<String>,
    /// Stable machine-readable reason when the runtime is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub loss_reason: Option<String>,
}

impl RuntimeState {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Live => "live",
            Self::Reconnecting => "reconnecting",
            Self::Terminal => "terminal",
            Self::Lost => "lost",
            Self::Conflict => "conflict",
            Self::Incompatible => "incompatible",
        }
    }
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWarningKind.ts"))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWarning.ts"))]
pub struct SessionWarning {
    /// Machine-readable warning kind.
    pub kind: SessionWarningKind,
    /// Human-readable summary of what happened and what was done instead.
    pub message: String,
    /// Optional longer detail (e.g. trimmed git stderr) for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub detail: Option<String>,
}

/// Mutation capabilities frozen for one logical session.
///
/// Clients use these flags to render resume and fork actions without inferring
/// provider behavior from [`SessionInfo::agent`] or [`SessionInfo::agent_base`].
/// A daemon loading a record that predates this field defaults both flags to
/// false, preserving the fail-closed persistence contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionCapabilities.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionCapabilities {
    /// Whether the session has a frozen native resume operation.
    pub resume: bool,
    /// Whether the session has a frozen native fork operation.
    pub fork: bool,
}

/// Summary returned by session lifecycle methods and published by events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionInfo.ts"))]
pub struct SessionInfo {
    /// Stable session identifier.
    pub id: SessionId,
    /// Whether this entry represents an observe-only process outside pohunek.
    ///
    /// `Some(false)` marks a normal PTY-backed session owned by the daemon.
    /// `Some(true)` marks an external agent observed by the opt-in external
    /// observer; those entries are read-only and have no PTY. `None` means the
    /// peer predates this additive field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub external: Option<bool>,
    /// Session-specific mutation capabilities frozen when the session started.
    #[serde(default)]
    pub capabilities: SessionCapabilities,
    /// Owner-set display name, or `None` when the session is shown by its id.
    /// Set at `session.new` and changed via `session.rename`; cosmetic only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    /// Agent profile name backing the session.
    pub agent: String,
    /// Resolved base kind backing the session.
    pub agent_base: AgentKind,
    /// Current working directory for the session.
    pub cwd: PathBuf,
    /// Source that last set [`Self::cwd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub cwd_source: Option<CwdSource>,
    /// Operating-system process id of the session root process.
    pub pid: u32,
    /// Durable worker runtime information.
    ///
    /// `None` means the peer predates worker-backed sessions or this is an
    /// observe-only external process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime: Option<SessionRuntime>,
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
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity: Option<AgentActivity>,
    /// Active nested agent profile name reported by a session-level hook.
    ///
    /// This is runtime metadata only: it does not change the launch identity in
    /// [`Self::agent`] and is cleared when the nested agent releases the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub active_agent: Option<String>,
    /// Resolved base kind for [`Self::active_agent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub active_agent_base: Option<AgentKind>,
    /// OS pid backing [`Self::active_agent`], when process facts have bound it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub active_agent_pid: Option<u32>,
    /// Native session id for the active nested agent, when reported.
    ///
    /// This metadata is distinct from [`Self::native_session_id`] and never acts
    /// as the parent session's resume binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub active_agent_session_id: Option<String>,
    /// Native session path for the active nested agent, when reported.
    ///
    /// This metadata is distinct from [`Self::native_session_path`] and never
    /// acts as the parent session's resume binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub active_agent_session_path: Option<String>,
    /// Native agent session id captured via the `SessionStart` hook, when one
    /// has been reported (see `docs/plan-phase-1.md` "Resume Model"). A session
    /// is resumable after a daemon restart only while this is present **and** the
    /// session is non-terminal: the daemon drops the resume binding on exit, so a
    /// terminal session can retain this id for reference yet not be resumable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub native_session_id: Option<String>,
    /// Native agent session **path** captured via the `SessionStart` hook, for an
    /// agent whose host profile resumes from a transcript path rather than an
    /// opaque id (`ref_kind = "path"`, Part C). Mutually exclusive with
    /// [`Self::native_session_id`]: a session resumes by exactly one of the two,
    /// chosen by its frozen `ref_kind`. `None` for the common id-resuming agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub native_session_path: Option<String>,
    /// Project this session belongs to, by derived id (`p-…`), when it started
    /// inside (or was pointed at) a git repository. `None` for a session with no
    /// git identity (a plain shell in a non-git directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub project_id: Option<String>,
    /// Current display label of [`Self::project_id`]'s project, **denormalized for
    /// display** and populated only by `session.list` (resolved fresh from the
    /// store at list time, so it reflects a rename). `None` for a session with no
    /// project or on responses that do not enrich it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub project_label: Option<String>,
    /// Whether the session's checkout is a linked git worktree rather than the
    /// repository's main checkout. `Some(true)` for a worktree-per-session, the
    /// detected value for an in-place session in a linked worktree, `Some(false)`
    /// for the main checkout, and `None` when the session has no git identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub is_linked_worktree: Option<bool>,
    /// Source git repository, when the session is bound to a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree, when the session has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub branch: Option<String>,
    /// Path to the bound worktree, when the session was launched in one. Equal
    /// to `cwd` for worktree sessions; absent for plain-`cwd` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
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
    #[cfg_attr(feature = "ts", ts(optional))]
    pub exit_code: Option<i32>,
}

/// Payload shared by session lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionEvent.ts"))]
pub struct SessionEvent {
    /// Session summary carried by the lifecycle event.
    pub session: SessionInfo,
}

/// Payload for `session_native_recovered`.
///
/// Native recovery preserves the logical session while replacing its PTY
/// runtime generation. The previous runtime may be absent for a session
/// imported from the one-time legacy migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "SessionNativeRecoveredEvent.ts")
)]
pub struct SessionNativeRecoveredEvent {
    /// Recovered logical session and its new runtime.
    pub session: SessionInfo,
    /// Runtime generation replaced by the explicit recovery, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub previous_runtime_id: Option<String>,
    /// Newly-created runtime generation, when the active backend exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime_id: Option<String>,
}

/// Payload for an `agent_state` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentStateEvent.ts"))]
pub struct AgentStateEvent {
    /// Session whose activity changed.
    pub session_id: SessionId,
    /// Current detected activity.
    pub activity: AgentActivity,
    /// Signal source that produced the activity value.
    pub source: StateSource,
    /// Runtime that produced this activity transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime: Option<SessionRuntimeIdentity>,
    /// Daemon epoch that scopes [`Self::revision`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity_epoch: Option<String>,
    /// Exact monotonic revision within [`Self::activity_epoch`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub revision: Option<ActivityRevision>,
}

/// Payload shared by attach lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AttachEvent.ts"))]
pub struct AttachEvent {
    /// Session owning the attach stream.
    pub session_id: SessionId,
    /// One-shot attach stream identifier.
    pub stream_id: String,
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionNewResult.ts"))]
pub struct SessionNewResult {
    /// The freshly created session.
    #[serde(flatten)]
    pub session: SessionInfo,
    /// `Some(true)` when the daemon applied an initial `input` in this same
    /// round-trip; absent when no initial input was requested or the daemon does
    /// not support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub applied_input: Option<bool>,
}

/// Result returned by `session.fork`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionForkResult.ts"))]
pub struct SessionForkResult {
    /// The freshly forked session.
    #[serde(flatten)]
    pub session: SessionInfo,
    /// Reserved for parity with session-creation flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub applied_input: Option<bool>,
}

/// Result returned by `session.stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionStopResult.ts"))]
pub struct SessionStopResult {
    /// Whether the daemon stopped a live session.
    pub stopped: bool,
}

/// Result returned by `session.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionResumeResult.ts"))]
pub struct SessionResumeResult {
    /// Relaunched session summary.
    pub session: SessionInfo,
}

/// Result returned by `session.remove`.
///
/// Removal is the one operation that makes a session truly disappear from the
/// daemon: `stop` only flips a live session to a terminal state, but the entry
/// lingers in the registry so `list`/`inspect` keep showing it. `remove` evicts
/// that entry, stopping a still-live session first so removal never orphans a
/// live PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionRemoveResult.ts"))]
pub struct SessionRemoveResult {
    /// Whether the daemon evicted a session entry from its registry. `false`
    /// only when a concurrent remove already evicted the same session.
    pub removed: bool,
    /// Whether a still-live session was stopped as part of this removal.
    pub stopped: bool,
}

/// Result returned by `session.resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionResizeResult.ts"))]
pub struct SessionResizeResult {
    /// Updated session summary after the resize.
    pub session: SessionInfo,
}

/// Parameters for `session.set_metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionSetMetadataParams.ts"))]
pub struct SessionSetMetadataParams {
    /// Session whose metadata should be merged.
    pub session_id: SessionId,
    /// Owner-controlled metadata patch. `Some(value)` sets a key and `None`
    /// removes it. Values must not contain secrets.
    pub metadata: BTreeMap<String, Option<String>>,
}

/// Result returned by `session.set_metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionSetMetadataResult.ts"))]
pub struct SessionSetMetadataResult {
    /// Updated session summary after the metadata merge.
    pub session: SessionInfo,
}

/// Parameters for `session.rename`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionRenameParams.ts"))]
pub struct SessionRenameParams {
    /// Session whose display name should change.
    pub session_id: SessionId,
    /// New display name. `Some(name)` sets it (trimmed by the daemon) and `None`
    /// clears it back to id-only display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
}

/// Result returned by `session.rename`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionRenameResult.ts"))]
pub struct SessionRenameResult {
    /// Updated session summary after the rename.
    pub session: SessionInfo,
}

/// Parameters for `session.diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionDiffParams.ts"))]
pub struct SessionDiffParams {
    /// Session whose worktree should be diffed against its base.
    pub session_id: SessionId,
    /// Explicit base ref to diff against. `None` defers to the worktree
    /// binding's recorded base branch, then the repository's default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub base: Option<String>,
}

/// Result returned by `session.diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionDiffResult.ts"))]
pub struct SessionDiffResult {
    /// Unified diff text of the session's worktree against `base`. May be
    /// truncated at a file boundary; see [`Self::truncated`].
    pub diff: String,
    /// Base ref the diff was actually computed against: the caller's explicit
    /// `base` when given, otherwise the resolved worktree/repository default.
    pub base: String,
    /// Whether `diff` was cut short at a file boundary to stay within
    /// [`crate::MAX_SESSION_DIFF_BYTES`]. When `true`, later files in the
    /// change set are omitted from `diff` entirely.
    pub truncated: bool,
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
            external: Some(false),
            capabilities: SessionCapabilities::default(),
            name: None,
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: PathBuf::from("/workspace"),
            cwd_source: Some(CwdSource::Launch),
            pid: 4242,
            runtime: None,
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: Some(AgentActivity::Working),
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
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
    fn cwd_source_strings_match_wire_repr() {
        assert_eq!(CwdSource::Launch.as_str(), "launch");
        assert_eq!(CwdSource::Procwatch.as_str(), "procwatch");
        assert_eq!(CwdSource::Osc7.as_str(), "osc7");
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
    fn agent_filter_matches_active_profile_name_or_base_kind() {
        let mut s = session("s-42");
        s.agent = "shell".to_owned();
        s.agent_base = AgentKind::Shell;
        s.active_agent = Some("codex-gpt-5".to_owned());
        s.active_agent_base = Some(AgentKind::Codex);

        assert!(SessionListFilter::Agent("shell".to_owned()).matches(&s));
        assert!(SessionListFilter::Agent("codex-gpt-5".to_owned()).matches(&s));
        assert!(SessionListFilter::Agent("codex".to_owned()).matches(&s));
        assert!(!SessionListFilter::Agent("claude".to_owned()).matches(&s));
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
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
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
            initial_dimensions: Some(TerminalDimensions::new(120, 40).expect("valid dimensions")),
            origin_session_id: Some(SessionId("s-1".to_owned())),
            origin_daemon_id: Some("daemon-abc".to_owned()),
            origin_worker_id: Some("worker-abc".to_owned()),
        };
        let value = serde_json::to_value(&with_origin).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "session_id": "s-1",
                "initial_dimensions": { "cols": 120, "rows": 40 },
                "origin_session_id": "s-1",
                "origin_daemon_id": "daemon-abc",
                "origin_worker_id": "worker-abc",
            })
        );
        let parsed: SessionAttachParams = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, with_origin);
    }

    #[test]
    fn attach_dimensions_reject_zero_axes() {
        assert!(matches!(
            TerminalDimensions::new(0, 24),
            Err(TerminalDimensionsError::Zero { cols: 0, rows: 24 })
        ));
        serde_json::from_value::<TerminalDimensions>(serde_json::json!({
            "cols": 80,
            "rows": 0,
        }))
        .expect_err("wire dimensions with a zero axis must be rejected");
    }

    #[test]
    fn session_event_payload_matches_legacy_json_payload() {
        let info = session("s-1");
        let typed = serde_json::to_value(SessionEvent {
            session: info.clone(),
        })
        .expect("serialize typed session event");
        let legacy = serde_json::json!({ "session": info });

        assert_eq!(
            serde_json::to_string(&typed).expect("typed json string"),
            serde_json::to_string(&legacy).expect("legacy json string")
        );
    }

    #[test]
    fn native_recovered_event_round_trips_runtime_generations() {
        let payload = SessionNativeRecoveredEvent {
            session: session("s-1"),
            previous_runtime_id: Some("runtime-old".to_owned()),
            runtime_id: Some("runtime-new".to_owned()),
        };
        let value = serde_json::to_value(&payload).expect("serialize recovery event");
        assert_eq!(value["previous_runtime_id"], "runtime-old");
        assert_eq!(value["runtime_id"], "runtime-new");
        assert_eq!(
            serde_json::from_value::<SessionNativeRecoveredEvent>(value)
                .expect("parse recovery event"),
            payload
        );
    }

    #[test]
    fn agent_state_event_payload_matches_json_payload() {
        let typed = serde_json::to_value(AgentStateEvent {
            session_id: SessionId("s-1".to_owned()),
            activity: AgentActivity::Working,
            source: StateSource::Report,
            runtime: Some(
                SessionRuntimeIdentity::new("runtime-1", RuntimeGeneration::new(2))
                    .expect("valid runtime identity"),
            ),
            activity_epoch: Some("d-epoch-1".to_owned()),
            revision: Some(ActivityRevision::new(3)),
        })
        .expect("serialize typed agent-state event");
        let legacy = serde_json::json!({
            "session_id": SessionId("s-1".to_owned()),
            "activity": AgentActivity::Working,
            "source": StateSource::Report,
            "runtime": {
                "runtime_id": "runtime-1",
                "runtime_generation": "2"
            },
            "activity_epoch": "d-epoch-1",
            "revision": "3",
        });

        assert_eq!(
            serde_json::to_string(&typed).expect("typed json string"),
            serde_json::to_string(&legacy).expect("legacy json string")
        );
    }

    #[test]
    fn agent_state_event_accepts_v2_payload_without_wait_evidence() {
        let event: AgentStateEvent = serde_json::from_value(serde_json::json!({
            "session_id": "s-1",
            "activity": "working",
            "source": "report",
        }))
        .expect("parse pre-evidence agent-state event");

        assert_eq!(event.runtime, None);
        assert_eq!(event.activity_epoch, None);
        assert_eq!(event.revision, None);
    }

    #[test]
    fn attach_event_payload_matches_legacy_json_payload() {
        let session_id = SessionId("s-1".to_owned());
        let typed = serde_json::to_value(AttachEvent {
            session_id: session_id.clone(),
            stream_id: "a-1".to_owned(),
        })
        .expect("serialize typed attach event");
        let legacy = serde_json::json!({
            "session_id": session_id,
            "stream_id": "a-1",
        });

        assert_eq!(
            serde_json::to_string(&typed).expect("typed json string"),
            serde_json::to_string(&legacy).expect("legacy json string")
        );
    }

    #[test]
    fn diff_params_omit_base_when_absent() {
        // Without an explicit base the wire form carries only the session id,
        // so an older daemon that predates `base` still parses the request.
        let bare = SessionDiffParams {
            session_id: SessionId("s-1".to_owned()),
            base: None,
        };
        let value = serde_json::to_value(&bare).expect("serialize");
        assert_eq!(value, serde_json::json!({ "session_id": "s-1" }));
        let parsed: SessionDiffParams =
            serde_json::from_value(serde_json::json!({ "session_id": "s-1" })).expect("parse");
        assert_eq!(parsed, bare);
    }

    #[test]
    fn diff_params_round_trip_with_explicit_base() {
        let with_base = SessionDiffParams {
            session_id: SessionId("s-1".to_owned()),
            base: Some("main".to_owned()),
        };
        let value = serde_json::to_value(&with_base).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({ "session_id": "s-1", "base": "main" })
        );
        let parsed: SessionDiffParams = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, with_base);
    }

    #[test]
    fn diff_result_round_trips_all_fields() {
        let result = SessionDiffResult {
            diff: "diff --git a/f b/f\n".to_owned(),
            base: "main".to_owned(),
            truncated: true,
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "diff": "diff --git a/f b/f\n",
                "base": "main",
                "truncated": true,
            })
        );
        let parsed: SessionDiffResult = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, result);
    }
}
