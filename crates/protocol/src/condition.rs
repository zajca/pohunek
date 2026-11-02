//! The one state a reader shows for a session, derived from fields a
//! protocol 4 daemon already sends (the recovery outlook needs the refusals of
//! a 0.35 or later daemon).
//!
//! Nothing here is on the wire: the condition, the recovery outlook and the
//! readable cause are pure projections of [`SessionInfo`], so a client shows
//! them against an older daemon as well.

// Rust guideline compliant 2026-11-02

use crate::session::{RuntimeExit, RuntimeState, SessionInfo, SessionRuntime, SessionState};

/// The state of a session as an operator reads it.
///
/// A session whose runtime was lost is [`Self::Crashed`] whether the loss is
/// proven (`failed`) or not (`running`): either way no PTY is left to attach
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionCondition {
    /// The daemon is starting the session.
    Starting,
    /// The session runs.
    Running,
    /// The session was stopped.
    Stopped,
    /// The session completed.
    Done,
    /// The session failed without losing its runtime.
    Failed,
    /// The runtime was lost: the worker or the host is gone.
    Crashed,
    /// The daemon is reconnecting to the worker.
    Reconnecting,
    /// More than one runtime identity claims the session.
    Conflict,
    /// The live worker speaks no compatible private protocol.
    Incompatible,
    /// The runtime state is one this build does not know; the session may
    /// still be live.
    Unknown,
}

impl SessionCondition {
    /// The label a list shows in its state column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Crashed => "crashed",
            Self::Reconnecting => "reconnecting",
            Self::Conflict => "conflict",
            Self::Incompatible => "incompatible",
            Self::Unknown => "unknown",
        }
    }

    const fn of_state(state: SessionState) -> Self {
        match state {
            SessionState::Starting => Self::Starting,
            SessionState::Running => Self::Running,
            SessionState::Stopped => Self::Stopped,
            SessionState::Done => Self::Done,
            SessionState::Failed => Self::Failed,
        }
    }
}

/// Whether a crashed session can be brought back with `session.resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionRecovery {
    /// A native resume is frozen for the session and the daemon does not
    /// refuse it.
    Recoverable,
    /// The daemon refuses the resume with this error code.
    Refused(String),
    /// The session froze no native resume.
    NoNativeResume,
    /// The payload cannot tell: only list and inspect responses carry the
    /// daemon's refusals.
    Unknown,
}

impl SessionInfo {
    /// The one state to show for the session.
    ///
    /// First match wins: an external entry keeps its state; a lost runtime is
    /// [`SessionCondition::Crashed`] whatever the state says; a terminal state
    /// is itself; a live state with a runtime that is reconnecting, in
    /// conflict or incompatible shows that runtime state, and with a runtime
    /// state this build does not know shows [`SessionCondition::Unknown`],
    /// never `running` and never `crashed`; everything else shows the state.
    #[must_use]
    pub fn condition(&self) -> SessionCondition {
        let state = SessionCondition::of_state(self.state);
        if self.external == Some(true) {
            return state;
        }
        let Some(runtime) = &self.runtime else {
            return state;
        };
        if runtime.state == RuntimeState::Lost {
            return SessionCondition::Crashed;
        }
        if self.state.is_terminal() {
            return state;
        }
        match runtime.state {
            RuntimeState::Reconnecting => SessionCondition::Reconnecting,
            RuntimeState::Conflict => SessionCondition::Conflict,
            RuntimeState::Incompatible => SessionCondition::Incompatible,
            RuntimeState::Unknown => SessionCondition::Unknown,
            _ => state,
        }
    }

    /// Whether a crashed session can be resumed; `None` for any other
    /// condition.
    ///
    /// `authoritative` is true for a list or inspect response, the only
    /// payloads that carry the daemon's refusals; an event payload cannot
    /// tell a refused resume from an allowed one.
    #[must_use]
    pub fn recovery(&self, authoritative: bool) -> Option<SessionRecovery> {
        if self.condition() != SessionCondition::Crashed {
            return None;
        }
        if !self.capabilities.resume {
            return Some(SessionRecovery::NoNativeResume);
        }
        let refusal = self
            .capability_refusals
            .as_ref()
            .and_then(|refusals| refusals.resume.clone());
        Some(match refusal {
            Some(code) => SessionRecovery::Refused(code),
            None if authoritative => SessionRecovery::Recoverable,
            None => SessionRecovery::Unknown,
        })
    }

    /// The readable cause of a lost runtime; `None` while the runtime is not
    /// lost.
    #[must_use]
    pub fn loss_message(&self) -> Option<String> {
        let runtime = self.runtime.as_ref()?;
        (runtime.state == RuntimeState::Lost).then(|| runtime.loss_message())
    }
}

impl SessionRuntime {
    /// The readable cause of the loss: the service manager's verdict when it
    /// recorded one, otherwise the daemon's explanation, otherwise the reason
    /// the loss was classified with. An unknown cause prints as received.
    #[must_use]
    pub fn loss_message(&self) -> String {
        let base = self.cause_message();
        if self.loss_reason.as_deref() == Some("runtime_lost_cleanup_unconfirmed")
            && (self.exit.is_some() || self.loss_cause.is_some())
        {
            return format!("{base}; processes of the session may still run, check ps");
        }
        base
    }

    fn cause_message(&self) -> String {
        if let Some(exit) = &self.exit {
            return exit.message();
        }
        if let Some(cause) = self.loss_cause.as_deref() {
            return match cause {
                "user_manager_lost" => {
                    "Lost with the user service manager; see journalctl -u user@<uid>.service"
                        .to_owned()
                }
                "host_restarted" => "Lost when the host restarted".to_owned(),
                other => other.to_owned(),
            };
        }
        match self.loss_reason.as_deref() {
            Some("runtime_lost") => "The worker ended and left no record of why".to_owned(),
            Some("runtime_lost_cleanup_unconfirmed") => {
                "The worker ended; processes of the session may still run, check ps".to_owned()
            }
            Some("worker_unavailable") => "No worker record exists for the session".to_owned(),
            Some(other) => other.to_owned(),
            None => "The session runtime was lost".to_owned(),
        }
    }
}

impl RuntimeExit {
    /// The readable end of the worker, such as "Killed by the kernel
    /// out-of-memory killer at 08:39 UTC (peak 26.3 GB)".
    #[must_use]
    pub fn message(&self) -> String {
        let mut text = match self.cause.as_str() {
            "oom_kill" => match self.oom_killer.as_deref() {
                Some("kernel") => "Killed by the kernel out-of-memory killer".to_owned(),
                Some("systemd_oomd") => "Killed by systemd-oomd (memory pressure)".to_owned(),
                Some("jetsam") => "Killed by macOS memory pressure (jetsam)".to_owned(),
                Some(other) => format!("Killed for memory by {other}"),
                None => "Killed for memory".to_owned(),
            },
            "signal" => match self.signal {
                Some(signal) => format!("Killed by signal {signal}"),
                None => "Killed by a signal".to_owned(),
            },
            "exit_code" => match self.code {
                Some(code) => format!("Worker exited with code {code}"),
                None => "Worker exited with an error".to_owned(),
            },
            "core_dump" => match self.signal {
                Some(signal) => format!("Worker dumped core on signal {signal}"),
                None => "Worker dumped core".to_owned(),
            },
            "timeout" => "Worker timed out".to_owned(),
            "watchdog" => "Worker stopped answering its watchdog".to_owned(),
            "resources" => "Worker hit a resource limit".to_owned(),
            "other" => "Worker ended for a reason the service manager did not classify".to_owned(),
            unknown => unknown.to_owned(),
        };
        if let Some(time) = self.ended_at.as_deref().and_then(clock_time) {
            text.push_str(" at ");
            text.push_str(&time);
        }
        if let Some(peak) = self.memory_peak_bytes {
            text = format!("{text} (peak {})", decimal_size(peak));
        }
        text
    }
}

/// `HH:MM` of an RFC 3339 timestamp, with `UTC` when it is written in UTC.
fn clock_time(timestamp: &str) -> Option<String> {
    let (_, time) = timestamp.split_once('T')?;
    let clock = time.get(..5)?;
    let valid = clock.as_bytes().iter().enumerate().all(|(index, byte)| {
        if index == 2 {
            *byte == b':'
        } else {
            byte.is_ascii_digit()
        }
    });
    if !valid {
        return None;
    }
    Some(if time.ends_with('Z') {
        format!("{clock} UTC")
    } else {
        clock.to_owned()
    })
}

/// A byte count in decimal units with one truncated decimal place, such as
/// `26.3 GB`.
fn decimal_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut scale: u128 = 1;
    let mut unit = 0;
    while u128::from(bytes) >= scale * 1000 && unit + 1 < UNITS.len() {
        scale *= 1000;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    let tenths = u128::from(bytes) * 10 / scale;
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}
