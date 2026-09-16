//! `pohunek agent-skill` — print the bundled agent skill.
//!
//! The skill is embedded at compile time from the checked-in generated artifact
//! (`agent_skill/SKILL.md`, rendered by `cargo xtask agent-skill generate` from
//! the hand-authored knowledge source `docs/knowledge/guides/agent-skill.md`).
//! The command is purely local: it never contacts a daemon, reads the
//! filesystem, or touches the network, and the global `--host` flag is accepted
//! but ignored.

// Rust guideline compliant 2026-09-16

use std::io::{self, Write};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::commands::render_json;
use crate::error::CliError;

/// The complete agent skill, embedded verbatim from the checked-in generated
/// artifact. The printed bytes are a compile-time constant, so one binary
/// version always prints identical output.
const EMBEDDED_SKILL: &str = include_str!("agent_skill/SKILL.md");

/// Print the embedded agent skill to stdout.
///
/// The default mode writes the embedded skill bytes verbatim, ending with the
/// artifact's single trailing newline. With `--json`, stdout carries exactly
/// one pretty-printed CLI process envelope (`skill` text plus its
/// `content_sha256`), so stdout stays machine-only; no human text is written.
///
/// # Errors
///
/// Returns [`CliError`] when the `--json` envelope cannot be serialized or
/// when a stdout write fails for a reason other than a closed consumer: a
/// reader that already went away (a truncated pipe such as
/// `pohunek agent-skill | head`) ends the command quietly instead of a
/// `print!` write panic.
pub(crate) fn run(json: bool) -> Result<(), CliError> {
    if json {
        let document = SkillDocument {
            skill: EMBEDDED_SKILL,
            content_sha256: content_sha256(),
        };
        return write_stdout(render_json(&document)?.as_bytes());
    }
    write_stdout(EMBEDDED_SKILL.as_bytes())
}

/// Writes the whole payload to locked stdout before flushing it.
fn write_stdout(payload: &[u8]) -> Result<(), CliError> {
    write_payload(&mut io::stdout().lock(), payload)
}

/// Writes one payload, treating a closed consumer as a successful early exit.
///
/// A piped reader can disappear at any moment (`pohunek agent-skill | head`);
/// that is the reader's choice, not a failure of this command, so it must not
/// surface as a panic, a stderr diagnostic, or a non-zero exit. Every other
/// write error is real and propagates as a typed [`CliError`].
fn write_payload(writer: &mut impl Write, payload: &[u8]) -> Result<(), CliError> {
    match writer.write_all(payload).and_then(|()| writer.flush()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(CliError::Io(error)),
    }
}

/// Lowercase-hex sha256 of the embedded skill bytes, the artifact identity
/// agents can use to verify or cache the printed skill.
fn content_sha256() -> String {
    format!("{:x}", Sha256::digest(EMBEDDED_SKILL.as_bytes()))
}

#[derive(Serialize)]
struct SkillDocument<'a> {
    skill: &'a str,
    content_sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_sha256_is_lowercase_hex_over_the_embedded_bytes() {
        let digest = content_sha256();
        assert_eq!(digest.len(), 64, "sha256 hex digest: {digest}");
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "digest must be lowercase hex: {digest}"
        );
    }

    #[test]
    fn json_document_serializes_skill_and_hash_fields() {
        let document = SkillDocument {
            skill: EMBEDDED_SKILL,
            content_sha256: content_sha256(),
        };
        let value: serde_json::Value =
            serde_json::to_value(&document).expect("serialize skill document");
        assert_eq!(value["skill"], EMBEDDED_SKILL);
        assert_eq!(value["content_sha256"], content_sha256());
    }

    #[test]
    fn write_payload_writes_and_flushes_the_whole_payload() {
        let mut buffer = Vec::new();

        write_payload(&mut buffer, EMBEDDED_SKILL.as_bytes()).expect("in-memory write succeeds");

        assert_eq!(buffer, EMBEDDED_SKILL.as_bytes());
    }

    #[test]
    fn write_payload_treats_closed_consumer_as_successful_early_exit() {
        let mut writer = FailingWriter {
            kind: io::ErrorKind::BrokenPipe,
            write_attempted: false,
        };

        write_payload(&mut writer, b"payload").expect("broken pipe ends quietly");

        assert!(writer.write_attempted);
    }

    #[test]
    fn write_payload_propagates_other_write_errors() {
        let mut writer = FailingWriter {
            kind: io::ErrorKind::PermissionDenied,
            write_attempted: false,
        };

        match write_payload(&mut writer, b"payload") {
            Err(CliError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected a typed io error, got {other:?}"),
        }
        assert!(writer.write_attempted);
    }

    /// In-memory writer that always fails with a fixed error kind, so
    /// broken-pipe and error propagation can be tested without a real closed
    /// stdout.
    struct FailingWriter {
        kind: io::ErrorKind,
        write_attempted: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            self.write_attempted = true;
            Err(io::Error::from(self.kind))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
