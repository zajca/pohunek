//! The latest placement escape check of live sessions, read from their
//! workers.
//!
//! A worker runs the check for a runtime that declares `[placement]` and keeps
//! its latest result in memory. The daemon reads it on demand for
//! `session.inspect` and `daemon.doctor`; nothing is stored or emitted.

// Rust guideline compliant 2026-11-06

use pohunek_worker_protocol::{PlacementOutcome, PlacementReport};
use protocol::{RuntimeState, SessionId, SessionPlacement, SessionPlacementProcess};

use super::{RuntimeHandle, SessionRegistry};
use crate::runtime::Worker;

impl SessionRegistry {
    /// The placement check result of session `id`, or `None` when the session
    /// is not live under a worker, its runtime declares no placement, or its
    /// worker cannot answer.
    pub(crate) async fn live_placement(&self, id: &SessionId) -> Option<SessionPlacement> {
        let worker = {
            let sessions = self.inner.sessions.lock().await;
            live_worker(sessions.get(id)?)?
        };
        report_of(&worker).await
    }

    /// The placement check result of every live session whose worker has one.
    pub(crate) async fn live_placements(&self) -> Vec<(SessionId, SessionPlacement)> {
        let workers: Vec<(SessionId, Worker)> = {
            let sessions = self.inner.sessions.lock().await;
            sessions
                .iter()
                .filter_map(|(id, entry)| Some((id.clone(), live_worker(entry)?)))
                .collect()
        };
        let mut placements = Vec::new();
        for (id, worker) in workers {
            if let Some(placement) = report_of(&worker).await {
                placements.push((id, placement));
            }
        }
        placements.sort_by(|left, right| left.0 .0.cmp(&right.0 .0));
        placements
    }
}

fn live_worker(entry: &super::SessionEntry) -> Option<Worker> {
    let live = entry
        .info
        .runtime
        .as_ref()
        .is_some_and(|runtime| runtime.state == RuntimeState::Live);
    match &entry.runtime {
        RuntimeHandle::Worker(worker) if live => Some(worker.clone()),
        _ => None,
    }
}

async fn report_of(worker: &Worker) -> Option<SessionPlacement> {
    let report = worker.inspect().await.ok()?.placement?;
    Some(public_placement(&report))
}

fn public_placement(report: &PlacementReport) -> SessionPlacement {
    SessionPlacement {
        outcome: match report.outcome {
            PlacementOutcome::Clean => "clean",
            PlacementOutcome::Escaped => "escaped",
            PlacementOutcome::Unobservable => "unobservable",
        }
        .to_owned(),
        processes: report
            .processes
            .iter()
            .map(|process| SessionPlacementProcess {
                pid: process.pid,
                comm: process.comm.clone(),
            })
            .collect(),
    }
}
