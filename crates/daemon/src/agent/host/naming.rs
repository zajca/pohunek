//! A runtime descriptor's `[naming] launch_args`: the arguments that hand a
//! session's pohunek name to the agent at launch.
//!
//! ```toml
//! [naming]
//! launch_args = ["--name={session_name}"]
//! ```
//!
//! Exactly one token holds the `{session_name}` placeholder, as the last part
//! of a `-<flag>=` prefix. The name is joined into that one element, so a name
//! that starts with a dash can never be parsed as an option, and the
//! placeholder is substituted in one pass, so the value is never scanned again.
//! The arguments are rendered at every launch from the name that launch uses
//! and are never part of the frozen resume snapshot.

// Rust guideline compliant 2026-10-27

use super::definition::{validate_token, DefinitionError, MAX_ARG_BYTES};
use crate::session::MAX_SESSION_NAME_BYTES;

/// Placeholder a `[naming]` token holds for the session name.
pub const SESSION_NAME_PLACEHOLDER: &str = "{session_name}";

/// Maximum number of tokens in `[naming] launch_args`.
///
/// One flag with its value is the expected shape; the rest is headroom for a
/// runtime that needs a companion flag.
pub const MAX_NAMING_TOKENS: usize = 8;

/// Descriptor field the diagnostics of this module name.
const FIELD: &str = "naming.launch_args";

/// The validated `[naming] launch_args` of a runtime; empty for a runtime
/// that declares none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamingLaunchArgs {
    tokens: Vec<String>,
}

impl NamingLaunchArgs {
    /// Validates the declared tokens.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::Field`] for no token or more than
    /// [`MAX_NAMING_TOKENS`], an empty, oversized or control-character token,
    /// a number of placeholders other than one, a brace outside the
    /// placeholder, a placeholder that does not end a `-<flag>=` token, or a
    /// prefix so long that a longest session name would overflow
    /// [`MAX_ARG_BYTES`]. A diagnostic never reflects a token.
    pub fn new(tokens: Vec<String>) -> Result<Self, DefinitionError> {
        if tokens.is_empty() || tokens.len() > MAX_NAMING_TOKENS {
            return Err(refuse("must hold 1..=8 tokens"));
        }
        for token in &tokens {
            validate_token(token, FIELD)?;
        }
        let placeholders: usize = tokens
            .iter()
            .map(|token| token.matches(SESSION_NAME_PLACEHOLDER).count())
            .sum();
        if placeholders != 1 {
            return Err(refuse("must hold exactly one {session_name} placeholder"));
        }
        if tokens.iter().any(|token| {
            token
                .replace(SESSION_NAME_PLACEHOLDER, "")
                .contains(['{', '}'])
        }) {
            return Err(refuse(
                "may contain braces only as the {session_name} placeholder",
            ));
        }
        let Some(prefix) = tokens
            .iter()
            .find_map(|token| token.strip_suffix(SESSION_NAME_PLACEHOLDER))
        else {
            return Err(refuse(
                "the {session_name} placeholder must end a `-<flag>=` token",
            ));
        };
        if !(prefix.starts_with('-') && prefix.ends_with('=')) {
            return Err(refuse(
                "the {session_name} placeholder must end a `-<flag>=` token",
            ));
        }
        if prefix.len() + MAX_SESSION_NAME_BYTES > MAX_ARG_BYTES {
            return Err(refuse("the placeholder prefix is too long"));
        }
        Ok(Self { tokens })
    }

    /// Whether the runtime declares no naming arguments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The declared tokens, placeholder included.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.tokens
    }

    /// The launch arguments for a session named `name`.
    ///
    /// `name` is a validated session name (at most [`MAX_SESSION_NAME_BYTES`]
    /// bytes), which [`Self::new`] guarantees fits the one joined token.
    #[must_use]
    pub fn render(&self, name: &str) -> Vec<String> {
        self.tokens
            .iter()
            .map(|token| match token.strip_suffix(SESSION_NAME_PLACEHOLDER) {
                Some(prefix) => format!("{prefix}{name}"),
                None => token.clone(),
            })
            .collect()
    }
}

fn refuse(reason: &'static str) -> DefinitionError {
    DefinitionError::Field {
        field: FIELD,
        reason,
    }
}
