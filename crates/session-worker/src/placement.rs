//! Placement escape check: finds processes of a runtime generation that left
//! the PTY's POSIX session as a daemon.
//!
//! A session's agent must run only inside the generation's own process tree.
//! An agent that prewarms a shared host daemon puts it in a session of its own
//! (`setsid`), and that daemon then outlives the generation and serves others.
//! The check reports such a process. It signals nothing: the stop and the
//! removal sweep already end every marked process, and a wrong kill of an
//! owner's daemon would cost more than a missed report.

// Rust guideline compliant 2026-11-06

use pohunek_platform::process::{Error as PlatformError, Pid, ProcessInspector, ProcessSession};
use pohunek_worker_protocol::{EscapedProcess, PlacementOutcome, PlacementReport};

/// Most escaped processes one report names. The list is a diagnostic; a
/// generation with more shows the first ones by process id.
pub(crate) const MAX_REPORTED_PROCESSES: usize = 8;

/// What the check needs to know about an armed runtime generation.
#[derive(Debug, Clone)]
pub(crate) struct Armed {
    /// POSIX session of the PTY child, read right after the spawn. `None` when
    /// the host could not report it, which makes every check unobservable.
    pub(crate) pty_session: Option<ProcessSession>,
    /// The generation's `POHUNEK_WORKER_INSTANCE_ID` marker.
    pub(crate) worker_instance_id: String,
    /// Process id of the worker itself, which is never a finding.
    pub(crate) worker_pid: Pid,
}

/// Runs one check over the same-user process table.
///
/// A process is an escape when it is outside the PTY's POSIX session, is named
/// for the provider (`named_for_provider`) or holds a listening socket, and
/// carries the generation's marker. The cheap tests run first so the
/// environment of an unrelated process is never read.
///
/// A process whose facts the host withholds cannot be attributed to the
/// generation and is skipped: another session's agent of the same provider
/// routinely hides its environment, and reporting every check unobservable for
/// it would hide the findings the check exists for. The report is unobservable
/// only when the host cannot answer the question at all, or when a process
/// that carries the marker cannot be classified.
pub(crate) fn survey(
    inspector: &dyn ProcessInspector,
    armed: &Armed,
    named_for_provider: &dyn Fn(Pid) -> bool,
) -> PlacementReport {
    let Some(pty_session) = armed.pty_session else {
        return unobservable();
    };
    let Ok(facts) = inspector.same_user_processes() else {
        return unobservable();
    };
    let mut escaped = Vec::new();
    let mut unclassified = 0_usize;
    for fact in facts {
        if fact.pid == armed.worker_pid {
            continue;
        }
        match inspector.process_session(fact.pid) {
            Ok(Some(session)) if session == pty_session => continue,
            Ok(Some(_)) => {}
            Err(PlatformError::Unavailable { .. }) => return unobservable(),
            Ok(None) | Err(_) => continue,
        }
        let mut listener_unknown = false;
        if !named_for_provider(fact.pid) {
            match inspector.holds_listener(fact.pid) {
                Ok(Some(true)) => {}
                Err(PlatformError::Unavailable { .. }) => return unobservable(),
                Ok(Some(false) | None) | Err(PlatformError::Race { .. }) => continue,
                Err(_) => listener_unknown = true,
            }
        }
        match inspector.ownership_markers(fact.pid) {
            Ok(markers)
                if markers.worker_instance_id.as_deref()
                    == Some(armed.worker_instance_id.as_str()) =>
            {
                if listener_unknown {
                    unclassified += 1;
                } else {
                    escaped.push(EscapedProcess {
                        pid: fact.pid,
                        comm: fact.comm,
                    });
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    if !escaped.is_empty() {
        escaped.sort_by_key(|process| process.pid);
        escaped.truncate(MAX_REPORTED_PROCESSES);
        return PlacementReport {
            outcome: PlacementOutcome::Escaped,
            processes: escaped,
        };
    }
    if unclassified > 0 {
        return unobservable();
    }
    PlacementReport {
        outcome: PlacementOutcome::Clean,
        processes: Vec::new(),
    }
}

fn unobservable() -> PlacementReport {
    PlacementReport {
        outcome: PlacementOutcome::Unobservable,
        processes: Vec::new(),
    }
}
