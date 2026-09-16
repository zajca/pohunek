//! `pohunek agent-skill` — print the bundled agent skill.
//!
//! The skill is embedded at compile time from the checked-in generated artifact
//! (`agent_skill/SKILL.md`, rendered by `cargo xtask agent-skill generate` from
//! the hand-authored knowledge source `docs/knowledge/guides/agent-skill.md`).
//! The command is purely local: it never contacts a daemon, reads the
//! filesystem, or touches the network, and the global `--host` flag is accepted
//! but ignored.

// Rust guideline compliant 2026-09-15

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
/// Returns [`CliError`] when the `--json` envelope cannot be serialized.
pub(crate) fn run(json: bool) -> Result<(), CliError> {
    if json {
        let document = SkillDocument {
            skill: EMBEDDED_SKILL,
            content_sha256: content_sha256(),
        };
        print!("{}", render_json(&document)?);
        return Ok(());
    }
    print!("{EMBEDDED_SKILL}");
    Ok(())
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
}
