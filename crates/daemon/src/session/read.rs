//! Bounded point-in-time terminal reads for managed sessions.

use std::time::Instant;

use protocol::{
    ErrorClass, ProtocolError, SessionReadFormat, SessionReadParams, SessionReadResult,
    SessionReadSource, TerminalWatermark, MAX_SESSION_READ_LINES, MAX_SESSION_READ_RESPONSE_BYTES,
};

use super::{
    observation_worker_error, runtime_identity, session_external_read_only, SessionRegistry,
};

impl SessionRegistry {
    /// Return a bounded terminal capture without taking attach ownership.
    pub(crate) async fn session_read(
        &self,
        params: &SessionReadParams,
    ) -> Result<SessionReadResult, ProtocolError> {
        let started = Instant::now();
        let id = params.session_id();
        if self.inner.external.contains_id(id).await {
            return Err(session_external_read_only(id));
        }

        let managed = match self.managed_session(id).await {
            Ok(managed) => managed,
            Err(error) if error.code == "session_has_no_managed_terminal" => {
                return Err(session_external_read_only(id));
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

        let line_limit = usize::try_from(effective_lines).unwrap_or(usize::MAX);
        let (lines, line_truncated) = tail_lines(snapshot.visible_lines, line_limit);
        let result = SessionReadResult {
            text: lines.join("\n"),
            source_used: available_source(requested_source),
            runtime: runtime_identity(managed.runtime_id, managed.runtime_generation)?,
            revision: TerminalWatermark::new(snapshot.watermark),
            alternate_screen: snapshot.alternate_screen,
            lines_requested: effective_lines,
            truncated: line_truncated,
        };
        let (result, byte_truncated) = bound_read_result(result)?;
        if byte_truncated {
            tracing::warn!(
                session_id = %id.0,
                response_bytes = serialized_len(&result)?,
                limit_bytes = MAX_SESSION_READ_RESPONSE_BYTES,
                "session read response was truncated to the serialized byte limit"
            );
        }
        tracing::debug!(
            session_id = %id.0,
            duration_ms = started.elapsed().as_millis(),
            source_requested = requested_source.as_str(),
            source_used = result.source_used.as_str(),
            alternate_screen = result.alternate_screen,
            lines_requested = result.lines_requested,
            response_bytes = serialized_len(&result)?,
            truncated = result.truncated,
            "session read completed"
        );
        Ok(result)
    }
}

const fn available_source(requested: SessionReadSource) -> SessionReadSource {
    match requested {
        SessionReadSource::Visible
        | SessionReadSource::Recent
        | SessionReadSource::RecentUnwrapped
        | SessionReadSource::Detection => SessionReadSource::Visible,
    }
}

fn tail_lines(mut lines: Vec<String>, limit: usize) -> (Vec<String>, bool) {
    let truncated = lines.len() > limit;
    if truncated {
        lines.drain(..lines.len() - limit);
    }
    (lines, truncated)
}

fn ansi_unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_read_ansi_unavailable",
        "ANSI session reads are unavailable from the current worker snapshot",
        None,
    )
}

fn bound_read_result(
    mut result: SessionReadResult,
) -> Result<(SessionReadResult, bool), ProtocolError> {
    if serialized_len(&result)? <= MAX_SESSION_READ_RESPONSE_BYTES {
        return Ok((result, false));
    }

    let text = std::mem::take(&mut result.text);
    result.truncated = true;
    let metadata_bytes = serialized_len(&result)?;
    if metadata_bytes > MAX_SESSION_READ_RESPONSE_BYTES {
        return Err(ProtocolError::session_output_limit_exceeded());
    }
    let text_budget = MAX_SESSION_READ_RESPONSE_BYTES - metadata_bytes;
    let start = json_suffix_start(&text, text_budget);
    text[start..].clone_into(&mut result.text);
    Ok((result, true))
}

fn serialized_len(result: &SessionReadResult) -> Result<usize, ProtocolError> {
    serde_json::to_vec(result)
        .map(|serialized| serialized.len())
        .map_err(|_error| {
            ProtocolError::new(
                ErrorClass::Runtime,
                "session_read_serialize_failed",
                "session read serialization failed",
                None,
            )
        })
}

fn json_suffix_start(text: &str, escaped_budget: usize) -> usize {
    let mut escaped_bytes = 0usize;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let character_bytes = json_escaped_char_len(character);
        if escaped_bytes.saturating_add(character_bytes) > escaped_budget {
            break;
        }
        escaped_bytes += character_bytes;
        start = index;
    }
    start
}

const fn json_escaped_char_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{0008}' | '\u{000C}' | '\n' | '\r' | '\t' => 2,
        '\u{0000}'..='\u{001F}' => 6,
        _ => character.len_utf8(),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{
        RuntimeGeneration, SessionReadResult, SessionReadSource, SessionRuntimeIdentity,
        TerminalWatermark,
    };

    use super::{
        available_source, bound_read_result, json_suffix_start, serialized_len, tail_lines,
        MAX_SESSION_READ_RESPONSE_BYTES,
    };

    fn result(text: String) -> SessionReadResult {
        SessionReadResult {
            text,
            source_used: SessionReadSource::Visible,
            runtime: SessionRuntimeIdentity::new("runtime-1", RuntimeGeneration::new(1))
                .expect("runtime identity"),
            revision: TerminalWatermark::new(1),
            alternate_screen: true,
            lines_requested: 1_000,
            truncated: false,
        }
    }

    #[test]
    fn read_result_uses_exact_serialized_bound_and_keeps_multibyte_tail() {
        let suffix = "tail-žluťoučký-界";
        let input = format!("{}{}", "\0".repeat(MAX_SESSION_READ_RESPONSE_BYTES), suffix);
        let (bounded, byte_truncated) = bound_read_result(result(input)).expect("bounded result");

        assert!(byte_truncated);
        assert!(bounded.truncated);
        assert!(bounded.text.ends_with(suffix));
        assert!(bounded.text.is_char_boundary(0));
        assert!(
            serialized_len(&bounded).expect("serialized length") <= MAX_SESSION_READ_RESPONSE_BYTES
        );
    }

    #[test]
    fn read_result_within_limit_is_unchanged() {
        let expected = result("plain".to_owned());
        let (actual, byte_truncated) = bound_read_result(expected.clone()).expect("bounded result");

        assert_eq!(actual, expected);
        assert!(!byte_truncated);
    }

    #[test]
    fn tail_lines_keep_newest_rows() {
        let (lines, truncated) = tail_lines(
            vec!["old".to_owned(), "middle".to_owned(), "new".to_owned()],
            2,
        );

        assert_eq!(lines, ["middle", "new"]);
        assert!(truncated);
    }

    #[test]
    fn unavailable_sources_report_visible_fallback() {
        for source in [
            SessionReadSource::Recent,
            SessionReadSource::RecentUnwrapped,
            SessionReadSource::Detection,
        ] {
            assert_eq!(available_source(source), SessionReadSource::Visible);
        }
    }

    #[test]
    fn escaped_suffix_budget_uses_json_lengths_and_utf8_boundaries() {
        let text = "head\n\0ž界";
        let suffix = "\0ž界";
        let escaped = serde_json::to_string(suffix).expect("serialize suffix");
        let start = json_suffix_start(text, escaped.len() - 2);

        assert_eq!(&text[start..], suffix);
    }
}
