//! Wire types of the bounded `session.wait_turn` method.
//!
//! A turn wait blocks until one of the targeted sessions records a turn event
//! (a finished turn, a blocked agent, a pending approval, or an exit) newer than
//! an opaque cursor, or until the requested timeout elapses. Every result carries
//! the next cursor and per-target progress, so waits chain without a gap and a
//! timeout still tells "working" from "stuck".

// Rust guideline compliant 2026-10-12

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    AgentActivity, NotificationId, SessionId, SessionState, MAX_SESSION_TURN_CURSOR_BYTES,
    MAX_SESSION_WAIT_TURN_TARGETS,
};

/// Reports an invalid `session.wait_turn` request.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TurnWaitParamsError {
    /// The request named no session.
    #[error("session_ids must name at least one session")]
    NoTargets,
    /// The request named more sessions than the protocol allows.
    #[error("session_ids must name at most {maximum} sessions, got {actual}")]
    TooManyTargets {
        /// Protocol maximum.
        maximum: usize,
        /// Requested count.
        actual: usize,
    },
    /// The request named one session twice.
    #[error("session_ids must not repeat a session")]
    DuplicateTarget,
    /// The request selected no turn event kind.
    #[error("until must select at least one turn event kind")]
    NoKinds,
    /// The request selected one turn event kind twice.
    #[error("until must not repeat a turn event kind")]
    DuplicateKind,
    /// The requested timeout was zero.
    #[error("timeout_ms must be nonzero")]
    ZeroTimeout,
    /// The cursor was empty, too long, or contained a non-printable character.
    #[error("after must be 1 to {maximum} printable ASCII bytes")]
    InvalidCursor {
        /// Protocol maximum in bytes.
        maximum: usize,
    },
}

/// A turn event kind a `session.wait_turn` request can wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionTurnEventKind.ts"))]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnEventKind {
    /// The agent finished a turn and is back at its prompt.
    TurnCompleted,
    /// The agent is blocked on the owner.
    AgentBlocked,
    /// The agent waits for an approval decision.
    ApprovalRequired,
    /// The session entered a terminal lifecycle state.
    Exited,
}

impl SessionTurnEventKind {
    /// Returns the wire spelling of the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnCompleted => "turn_completed",
            Self::AgentBlocked => "agent_blocked",
            Self::ApprovalRequired => "approval_required",
            Self::Exited => "exited",
        }
    }
}

impl fmt::Display for SessionTurnEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opaque position in a daemon's turn-event sequence.
///
/// Clients must treat the value as opaque: store it, pass it back as `after`,
/// and never parse or construct it. A cursor is meaningful only to the daemon
/// instance that issued it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionTurnCursor.ts"))]
pub struct SessionTurnCursor(String);

impl SessionTurnCursor {
    /// Creates a cursor from its wire spelling.
    ///
    /// # Errors
    ///
    /// Returns [`TurnWaitParamsError::InvalidCursor`] when the value is empty,
    /// longer than [`MAX_SESSION_TURN_CURSOR_BYTES`], or contains a byte that
    /// is not printable ASCII.
    pub fn new(value: impl Into<String>) -> Result<Self, TurnWaitParamsError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_SESSION_TURN_CURSOR_BYTES
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(TurnWaitParamsError::InvalidCursor {
                maximum: MAX_SESSION_TURN_CURSOR_BYTES,
            });
        }
        Ok(Self(value))
    }

    /// Returns the wire spelling of the cursor.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionTurnCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionTurnCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Parameters for the bounded `session.wait_turn` method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWaitTurnParams.ts"))]
pub struct SessionWaitTurnParams {
    /// Sessions whose turn events complete the wait.
    session_ids: Vec<SessionId>,
    /// Turn event kinds that complete the wait.
    until: Vec<SessionTurnEventKind>,
    /// Cursor of a previous result; only newer events complete the wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    after: Option<SessionTurnCursor>,
    /// Wait duration in milliseconds; the daemon default applies when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    timeout_ms: Option<u32>,
}

impl SessionWaitTurnParams {
    /// Creates a validated turn wait.
    ///
    /// The timeout is not compared with a ceiling here: the ceiling is a daemon
    /// setting, and a request above it fails with `session_wait_limit_exceeded`.
    ///
    /// # Errors
    ///
    /// Returns [`TurnWaitParamsError`] when `session_ids` is empty, longer than
    /// [`MAX_SESSION_WAIT_TURN_TARGETS`], or repeats a session, when `until` is
    /// empty or repeats a kind, or when `timeout_ms` is zero.
    pub fn new(
        session_ids: Vec<SessionId>,
        until: Vec<SessionTurnEventKind>,
        after: Option<SessionTurnCursor>,
        timeout_ms: Option<u32>,
    ) -> Result<Self, TurnWaitParamsError> {
        if session_ids.is_empty() {
            return Err(TurnWaitParamsError::NoTargets);
        }
        if session_ids.len() > MAX_SESSION_WAIT_TURN_TARGETS {
            return Err(TurnWaitParamsError::TooManyTargets {
                maximum: MAX_SESSION_WAIT_TURN_TARGETS,
                actual: session_ids.len(),
            });
        }
        if has_duplicate(&session_ids) {
            return Err(TurnWaitParamsError::DuplicateTarget);
        }
        if until.is_empty() {
            return Err(TurnWaitParamsError::NoKinds);
        }
        if has_duplicate(&until) {
            return Err(TurnWaitParamsError::DuplicateKind);
        }
        if timeout_ms == Some(0) {
            return Err(TurnWaitParamsError::ZeroTimeout);
        }
        Ok(Self {
            session_ids,
            until,
            after,
            timeout_ms,
        })
    }

    /// Returns the targeted sessions in request order.
    #[must_use]
    pub fn session_ids(&self) -> &[SessionId] {
        &self.session_ids
    }

    /// Returns the turn event kinds that complete the wait.
    #[must_use]
    pub fn until(&self) -> &[SessionTurnEventKind] {
        &self.until
    }

    /// Returns the cursor after which events count.
    #[must_use]
    pub const fn after(&self) -> Option<&SessionTurnCursor> {
        self.after.as_ref()
    }

    /// Returns the requested wait duration, if the request set one.
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u32> {
        self.timeout_ms
    }
}

impl<'de> Deserialize<'de> for SessionWaitTurnParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireParams {
            session_ids: Vec<SessionId>,
            until: Vec<SessionTurnEventKind>,
            #[serde(default)]
            after: Option<SessionTurnCursor>,
            #[serde(default)]
            timeout_ms: Option<u32>,
        }
        let wire = WireParams::deserialize(deserializer)?;
        Self::new(wire.session_ids, wire.until, wire.after, wire.timeout_ms)
            .map_err(serde::de::Error::custom)
    }
}

fn has_duplicate<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

/// Reason a bounded `session.wait_turn` request completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWaitTurnReason.ts"))]
#[serde(rename_all = "snake_case", remote = "Self")]
pub enum SessionWaitTurnReason {
    /// A selected turn event newer than the cursor was recorded.
    Event,
    /// The wait duration elapsed without a selected event.
    Timeout,
    /// The cursor came from another daemon instance or is older than the
    /// retained turn events; the result carries a fresh cursor.
    CursorReset,
    /// Every target is in a terminal state and none has a newer selected event.
    TargetsEnded,
    /// A value a newer daemon sent that this build does not know. Only the
    /// client's [`tolerant`](crate::tolerant) decode produces it; plain decoding
    /// rejects the value and the daemon never holds or emits one.
    #[doc(hidden)]
    #[serde(rename = "unknown", skip_deserializing)]
    #[cfg_attr(feature = "ts", ts(skip))]
    Unknown,
}

crate::tolerant_enum!(SessionWaitTurnReason);

/// One recorded turn event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionTurnEvent.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionTurnEvent {
    /// Session the event belongs to.
    pub session_id: SessionId,
    /// Kind of the event.
    pub kind: SessionTurnEventKind,
    /// RFC 3339 time the daemon recorded the event, after any debounce.
    pub at: String,
    /// Inbox notification recorded for the same event, when the notification
    /// policy kept one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub notification_id: Option<NotificationId>,
}

/// Current progress of one targeted session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionTurnProgress.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionTurnProgress {
    /// Targeted session.
    pub session_id: SessionId,
    /// Current lifecycle state.
    pub state: SessionState,
    /// Current agent activity, when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity: Option<AgentActivity>,
    /// RFC 3339 time of the last terminal output this daemon observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub last_output_at: Option<String>,
    /// Whether terminal output arrived after the request's cursor was issued,
    /// or after the request started when it carried no cursor.
    pub output_advanced: bool,
    /// RFC 3339 time the agent last entered `working`, when this daemon
    /// observed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub turn_started_at: Option<String>,
}

/// Result returned by the bounded `session.wait_turn` method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionWaitTurnResult.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionWaitTurnResult {
    /// Why the wait completed.
    pub reason: SessionWaitTurnReason,
    /// The event that completed the wait; present exactly when `reason` is
    /// `event`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub event: Option<SessionTurnEvent>,
    /// Cursor to pass as `after` to the next wait.
    pub cursor: SessionTurnCursor,
    /// Progress of every target, in request order.
    pub progress: Vec<SessionTurnProgress>,
}
