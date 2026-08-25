//! Bounded point-in-time terminal reads for managed sessions.

use protocol::{
    ErrorClass, ProtocolError, SessionReadFormat, SessionReadParams, SessionReadResult,
    SessionReadSource, MAX_SESSION_READ_LINES,
};

use super::{session_not_found, session_not_running, RuntimeHandle, SessionId, SessionRegistry};

/// Maximum terminal rows retained by the daemon's screen tracker.
///
/// This bounds recent captures without introducing another configurable limit.
const MAX_READ_ROWS: usize = MAX_SESSION_READ_LINES as usize;

impl SessionRegistry {
    /// Return a bounded terminal capture without taking attach ownership.
    pub(crate) async fn session_read(
        &self,
        params: &SessionReadParams,
    ) -> Result<SessionReadResult, ProtocolError> {
        let id = params.session_id();
        if self.inner.external.contains_id(id).await {
            return Err(external_read_only(id));
        }
        let (worker, runtime_state) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            let RuntimeHandle::Worker(worker) = &entry.runtime else {
                return Err(session_not_running(id));
            };
            (worker.clone(), entry.info.state)
        };
        if runtime_state != protocol::SessionState::Running {
            return Err(session_not_running(id));
        }

        let snapshot = worker
            .terminal_snapshot()
            .await
            .map_err(|_error| ProtocolError::session_terminal_unavailable())?;
        self.verify_read_identity(id, snapshot.watermark).await?;
        let requested_source = params.source().unwrap_or(SessionReadSource::Visible);
        let requested_format = params.format().unwrap_or(SessionReadFormat::Text);
        if matches!(requested_format, SessionReadFormat::Ansi) {
            return Err(ansi_unavailable());
        }

        let mut lines = match requested_source {
            SessionReadSource::Visible => snapshot.visible_lines,
            SessionReadSource::Recent | SessionReadSource::RecentUnwrapped => {
                tail_lines(snapshot.visible_lines, params.lines())
            }
            SessionReadSource::Detection => {
                let detection = snapshot
                    .visible_lines
                    .iter()
                    .map(|line| line.trim_end().to_owned())
                    .collect();
                tail_lines(detection, params.lines())
            }
        };
        if requested_source == SessionReadSource::RecentUnwrapped {
            for line in &mut lines {
                *line = line.replace('\n', " ");
            }
        }
        let truncated = lines.len()
            > usize::try_from(params.lines().unwrap_or(MAX_SESSION_READ_LINES))
                .unwrap_or(usize::MAX);
        lines.truncate(MAX_READ_ROWS);
        Ok(SessionReadResult {
            text: lines.join("\n"),
            source_used: requested_source,
            lines_requested: params.lines().unwrap_or(MAX_SESSION_READ_LINES),
            truncated,
            revision: snapshot.watermark,
        })
    }

    async fn verify_read_identity(
        &self,
        id: &SessionId,
        observed_revision: u64,
    ) -> Result<(), ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
        let RuntimeHandle::Worker(_worker) = &entry.runtime else {
            return Err(session_not_running(id));
        };
        if !entry
            .info
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == protocol::RuntimeState::Live)
        {
            return Err(session_not_running(id));
        }
        let _ = observed_revision;
        Ok(())
    }
}

fn tail_lines(mut lines: Vec<String>, requested: Option<u32>) -> Vec<String> {
    let count = usize::try_from(requested.unwrap_or(protocol::MAX_SESSION_READ_LINES))
        .unwrap_or(usize::MAX);
    if lines.len() > count {
        let start = lines.len() - count;
        lines.drain(..start);
    }
    lines
}

fn external_read_only(id: &SessionId) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_external_read_only",
        format!("session {} is an external observe-only agent", id.0),
        None,
    )
}

fn ansi_unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_read_ansi_unavailable",
        "ANSI session reads are unavailable from the current worker snapshot",
        None,
    )
}
