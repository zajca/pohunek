//! Bounded point-in-time terminal reads for managed sessions.

use protocol::{
    ErrorClass, ProtocolError, SessionReadFormat, SessionReadParams, SessionReadResult,
    SessionReadSource, TerminalWatermark, MAX_SESSION_READ_LINES, MAX_SESSION_READ_RESPONSE_BYTES,
};

use super::{observation_worker_error, runtime_identity, SessionId, SessionRegistry};

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

        let managed = match self.managed_session(id).await {
            Ok(managed) => managed,
            Err(error) if error.code == "session_has_no_managed_terminal" => {
                return Err(external_read_only(id));
            }
            Err(error) => return Err(error),
        };

        let snapshot = managed
            .worker
            .terminal_snapshot()
            .await
            .map_err(observation_worker_error)?;
        self.verify_managed_identity(id, &managed).await?;

        let requested_source = params.source().unwrap_or(SessionReadSource::Visible);
        let requested_format = params.format().unwrap_or(SessionReadFormat::Text);
        if matches!(requested_format, SessionReadFormat::Ansi) {
            return Err(ansi_unavailable());
        }
        let effective_lines = params.lines().unwrap_or(MAX_SESSION_READ_LINES);

        let mut alternate_screen = snapshot.alternate_screen;
        let source_used = requested_source;
        if requested_source == SessionReadSource::Recent {
            alternate_screen = false;
        }
        let mut lines = snapshot.visible_lines;
        if requested_source == SessionReadSource::Detection {
            for line in &mut lines {
                *line = line.trim_end().to_owned();
            }
        } else if requested_source == SessionReadSource::RecentUnwrapped {
            alternate_screen = false;
            for line in &mut lines {
                *line = line.replace('\n', " ");
            }
        }
        let mut truncated = lines.len() > usize::try_from(effective_lines).unwrap_or(usize::MAX);
        lines.truncate(usize::try_from(effective_lines).unwrap_or(usize::MAX));
        Ok(SessionReadResult {
            text: truncate_read_text(lines.join("\n"), &mut truncated),
            source_used,
            runtime: runtime_identity(managed.runtime_id, managed.runtime_generation)?,
            revision: TerminalWatermark::new(snapshot.watermark),
            alternate_screen,
            lines_requested: effective_lines,
            truncated,
        })
    }
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

fn truncate_read_text(mut text: String, truncated: &mut bool) -> String {
    while serde_json::to_string(&text)
        .is_ok_and(|encoded| encoded.len() + 2 > MAX_SESSION_READ_RESPONSE_BYTES)
    {
        *truncated = true;
        let safe_end = std::str::from_utf8(&text.as_bytes()[..text.len() - 1])
            .map_or(text.len() - 1, str::len);
        text.truncate(safe_end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{truncate_read_text, MAX_SESSION_READ_RESPONSE_BYTES};

    #[test]
    fn read_text_truncates_on_serialized_bytes_and_sets_truncated() {
        let mut truncated = false;
        let text = truncate_read_text("\0".repeat(MAX_SESSION_READ_RESPONSE_BYTES), &mut truncated);
        assert!(truncated);
        let encoded = serde_json::to_string(&text).expect("serialize truncated text");
        assert!(encoded.len() <= MAX_SESSION_READ_RESPONSE_BYTES);
    }

    #[test]
    fn read_text_within_limit_does_not_set_truncated() {
        let mut truncated = false;
        let text = truncate_read_text("plain".to_owned(), &mut truncated);
        assert_eq!(text, "plain");
        assert!(!truncated);
    }
}
