//! Rendering of the compiled reporter script templates for one runtime.
//!
//! A reporter script template carries the runtime-specific values as
//! placeholders; the integration handler renders it from the runtime's id and
//! display name before the script is staged, so the installed bytes, the
//! drift comparison against them, and the ownership marker all derive from the
//! rendered text. Both values end up inside shell comments, a shell double
//! quoted temp-file path and Python string literals of the script, so they are
//! restricted to a character set none of those contexts interpret.

// Rust guideline compliant 2026-10-05

use protocol::{RuntimeId, RuntimeIdError};
use thiserror::Error;

/// Placeholder replaced by the runtime id.
const ID_PLACEHOLDER: &str = "@POHUNEK_AGENT_ID@";
/// Placeholder replaced by the runtime display name.
const NAME_PLACEHOLDER: &str = "@POHUNEK_AGENT_NAME@";
/// Prefix every placeholder shares; text starting with it that survives
/// rendering is an unknown placeholder.
const PLACEHOLDER_PREFIX: &str = "@POHUNEK_";

/// Longest display name, in bytes.
///
/// Keeps the notification text and the script header readable; the value has
/// no effect on parsing, so raising it only lengthens those lines.
const MAX_NAME_BYTES: usize = 64;

/// Characters besides ASCII alphanumerics a display name may contain. None is
/// special to a POSIX shell comment, a shell double-quoted string, a
/// Python string literal, or the placeholder syntax.
const NAME_PUNCTUATION: &[u8] = b" ._-";

/// Why a reporter template cannot be rendered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(super) enum ReporterRenderError {
    /// The runtime id does not match the runtime id grammar.
    #[error("reporter runtime id is invalid: {0}")]
    InvalidId(#[from] RuntimeIdError),
    /// The display name is empty or longer than the accepted size.
    #[error("reporter display name must be 1..={MAX_NAME_BYTES} bytes")]
    NameSize,
    /// The display name has a character outside ASCII alphanumerics and
    /// space, `.`, `_`, `-`, or starts or ends with a space.
    #[error("reporter display name has a character outside [A-Za-z0-9 ._-] at byte {index}")]
    NameCharacter {
        /// Byte offset of the first rejected character.
        index: usize,
    },
    /// The template still holds a placeholder after substitution.
    #[error("reporter template holds an unresolved placeholder at byte {index}")]
    UnresolvedPlaceholder {
        /// Byte offset of the placeholder in the rendered text.
        index: usize,
    },
}

/// The runtime values a reporter script is rendered with, valid by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReporterIdentity {
    id: RuntimeId,
    name: String,
}

impl ReporterIdentity {
    /// Validates the runtime id and display name of a descriptor.
    ///
    /// # Errors
    ///
    /// Returns the typed reason the id or the name is unsafe to embed in a
    /// script.
    pub(super) fn new(id: &str, name: &str) -> Result<Self, ReporterRenderError> {
        let id = RuntimeId::parse(id)?;
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(ReporterRenderError::NameSize);
        }
        if let Some(index) = name
            .bytes()
            .position(|byte| !(byte.is_ascii_alphanumeric() || NAME_PUNCTUATION.contains(&byte)))
        {
            return Err(ReporterRenderError::NameCharacter { index });
        }
        if name.starts_with(' ') {
            return Err(ReporterRenderError::NameCharacter { index: 0 });
        }
        if name.ends_with(' ') {
            return Err(ReporterRenderError::NameCharacter {
                index: name.len() - 1,
            });
        }
        Ok(Self {
            id,
            name: name.to_owned(),
        })
    }

    /// Renders `template` with this identity.
    ///
    /// # Errors
    ///
    /// Returns [`ReporterRenderError::UnresolvedPlaceholder`] when the template
    /// names a placeholder this renderer does not know.
    pub(super) fn render(&self, template: &str) -> Result<String, ReporterRenderError> {
        let rendered = template
            .replace(ID_PLACEHOLDER, self.id.as_str())
            .replace(NAME_PLACEHOLDER, &self.name);
        match rendered.find(PLACEHOLDER_PREFIX) {
            Some(index) => Err(ReporterRenderError::UnresolvedPlaceholder { index }),
            None => Ok(rendered),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE_TEMPLATE: &str = include_str!("assets/codex/pohunek-agent-state.sh");
    const NOTIFY_TEMPLATE: &str = include_str!("assets/codex/pohunek-agent-notify.sh");
    const GOLDEN_STATE: &str = include_str!("reporter_golden/codex-pohunek-agent-state.sh");
    const GOLDEN_NOTIFY: &str = include_str!("reporter_golden/codex-pohunek-agent-notify.sh");

    fn codex_identity() -> ReporterIdentity {
        ReporterIdentity::new("codex", "Codex").expect("valid identity")
    }

    #[test]
    fn the_rendered_templates_equal_the_recorded_scripts_byte_for_byte() {
        let identity = codex_identity();

        assert_eq!(
            identity.render(STATE_TEMPLATE).expect("render").as_bytes(),
            GOLDEN_STATE.as_bytes()
        );
        assert_eq!(
            identity.render(NOTIFY_TEMPLATE).expect("render").as_bytes(),
            GOLDEN_NOTIFY.as_bytes()
        );
    }

    #[test]
    fn the_templates_hold_no_runtime_specific_text() {
        for (role, template) in [("state", STATE_TEMPLATE), ("notify", NOTIFY_TEMPLATE)] {
            let lowered = template.to_ascii_lowercase();
            assert!(!lowered.contains("codex"), "{role} template names codex");
            assert!(!lowered.contains("claude"), "{role} template names claude");
        }
    }

    #[test]
    fn another_runtime_renders_a_distinct_script_without_leaking_the_first() {
        let other = ReporterIdentity::new("pohunek.runtime-x_2", "Acme Agent 2.0").expect("valid");

        for template in [STATE_TEMPLATE, NOTIFY_TEMPLATE] {
            let rendered = other.render(template).expect("render");
            assert!(!rendered.to_ascii_lowercase().contains("codex"));
            assert!(!rendered.contains("@POHUNEK_"));
            assert_ne!(rendered, codex_identity().render(template).expect("render"));
        }
        let notify = other.render(NOTIFY_TEMPLATE).expect("render");
        assert!(notify.contains("# POHUNEK_INTEGRATION_ID=pohunek.runtime-x_2\n"));
        assert!(notify.contains("AGENT = \"pohunek.runtime-x_2\"\n"));
        assert!(notify.contains("\"title\": \"Acme Agent 2.0 approval required\""));
        assert!(notify.contains("/pohunek-pohunek.runtime-x_2-notify.XXXXXX"));
        let state = other.render(STATE_TEMPLATE).expect("render");
        assert!(state.contains("agent = \"pohunek.runtime-x_2\"\n"));
    }

    #[test]
    fn hostile_names_are_refused() {
        for hostile in [
            "a\"b",
            "a'b",
            "$(id)",
            "a`id`",
            "a\nb",
            "a\rb",
            "a\0b",
            "a\\b",
            "a$b",
            "a;b",
            "a@POHUNEK_AGENT_ID@",
            "a%b",
            "a{b}",
            "caf\u{e9}",
            " lead",
            "trail ",
            "",
        ] {
            assert!(
                ReporterIdentity::new("codex", hostile).is_err(),
                "{hostile:?} must be refused"
            );
        }
        assert_eq!(
            ReporterIdentity::new("codex", &"a".repeat(MAX_NAME_BYTES + 1)),
            Err(ReporterRenderError::NameSize)
        );
        ReporterIdentity::new("codex", &"a".repeat(MAX_NAME_BYTES))
            .expect("a name of the maximum size");
    }

    #[test]
    fn hostile_ids_are_refused() {
        for hostile in [
            "a\"b", "a'b", "$(id)", "a`id`", "a\nb", "a\0b", "A", "a b", "a/b", "a@b", "", "-lead",
            ".lead", "a..b",
        ] {
            assert!(
                matches!(
                    ReporterIdentity::new(hostile, "Name"),
                    Err(ReporterRenderError::InvalidId(_))
                ),
                "{hostile:?} must be refused"
            );
        }
    }

    #[test]
    fn an_unknown_placeholder_fails_the_render() {
        let identity = codex_identity();

        assert_eq!(
            identity.render("a @POHUNEK_AGENT_ID@ b @POHUNEK_OTHER@"),
            Err(ReporterRenderError::UnresolvedPlaceholder { index: 10 })
        );
    }

    #[test]
    fn substituted_values_are_not_rescanned_for_placeholders() {
        let identity = ReporterIdentity::new("codex", "Codex").expect("valid");

        assert_eq!(
            identity.render("@POHUNEK_AGENT_NAME@/@POHUNEK_AGENT_ID@"),
            Ok("Codex/codex".to_owned())
        );
    }
}
