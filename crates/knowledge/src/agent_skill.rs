//! The bundled Pohunek agent skill.
//!
//! The text is the checked-in generated artifact
//! (`agent_skill/SKILL.md`, rendered by `cargo xtask agent-skill generate` from
//! `docs/knowledge/guides/agent-skill.md`). `pohunek agent-skill` prints it and
//! the daemon installs it, so both emit the same bytes and the same digest.

// Rust guideline compliant 2026-11-11

use sha2::{Digest as _, Sha256};

/// The complete agent skill, embedded verbatim from the generated artifact.
pub const AGENT_SKILL: &str = include_str!("agent_skill/SKILL.md");

/// Lowercase-hex sha256 of [`AGENT_SKILL`], the artifact identity agents and
/// the installed-skill marker use to verify the text.
#[must_use]
pub fn agent_skill_sha256() -> String {
    format!("{:x}", Sha256::digest(AGENT_SKILL.as_bytes()))
}
