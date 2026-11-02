//! A runtime descriptor's `[naming] launch_args`: the arguments that hand a
//! session's pohunek name to the agent at launch.
//!
//! ```toml
//! [naming]
//! launch_args = ["--name={session_name}"]
//! ```
//!
//! Exactly one token holds the `{session_name}` placeholder, in one of two
//! shapes:
//!
//! - joined: the last part of a `-<flag>=` prefix (`--name={session_name}`).
//!   The name is joined into that one element, so a name that starts with a
//!   dash can never be parsed as an option.
//! - pair: the whole token, directly after a flag token
//!   (`["--name", "{session_name}"]`), for an agent that reads the value of a
//!   flag from the next element only. The name is a separate element there, so
//!   a name that starts with a dash is withheld: [`NamingLaunchArgs::render`]
//!   returns no arguments for it.
//!
//! The placeholder is substituted in one pass, so the value is never scanned
//! again. The arguments are rendered at every launch from the name that launch
//! uses and are never part of the frozen resume snapshot.

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
    /// Whether the placeholder is a whole token after a flag token.
    pair: bool,
}

impl NamingLaunchArgs {
    /// Validates the declared tokens.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::Field`] for no token or more than
    /// [`MAX_NAMING_TOKENS`], an empty, oversized or control-character token,
    /// a number of placeholders other than one, a brace outside the
    /// placeholder, a placeholder that neither ends a `-<flag>=` token nor is
    /// a whole token after a flag token, or a joined prefix so long that a
    /// longest session name would overflow [`MAX_ARG_BYTES`]. A diagnostic
    /// never reflects a token.
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
        let Some(position) = tokens
            .iter()
            .position(|token| token.contains(SESSION_NAME_PLACEHOLDER))
        else {
            return Err(refuse(PLACEMENT));
        };
        let pair = tokens[position] == SESSION_NAME_PLACEHOLDER;
        if pair {
            if position == 0 || !is_flag(&tokens[position - 1]) {
                return Err(refuse(PLACEMENT));
            }
        } else {
            let prefix = tokens[position]
                .strip_suffix(SESSION_NAME_PLACEHOLDER)
                .filter(|prefix| prefix.starts_with('-') && prefix.ends_with('='))
                .ok_or_else(|| refuse(PLACEMENT))?;
            if prefix.len() + MAX_SESSION_NAME_BYTES > MAX_ARG_BYTES {
                return Err(refuse("the placeholder prefix is too long"));
            }
        }
        Ok(Self { tokens, pair })
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

    /// The launch arguments for a session named `name`, or `None` when the
    /// pair form withholds a name that starts with a dash: as a separate
    /// element it could be read as an option of the agent. Nothing is
    /// withheld for a runtime that declares no arguments, nor in the joined
    /// form, where the name is one element with its flag.
    ///
    /// `name` is a validated session name (at most [`MAX_SESSION_NAME_BYTES`]
    /// bytes), which [`Self::new`] guarantees fits the one joined token.
    #[must_use]
    pub fn render(&self, name: &str) -> Option<Vec<String>> {
        if self.pair && name.starts_with('-') {
            return None;
        }
        Some(self.render_tokens(name))
    }

    fn render_tokens(&self, name: &str) -> Vec<String> {
        self.tokens
            .iter()
            .map(|token| match token.strip_suffix(SESSION_NAME_PLACEHOLDER) {
                Some(prefix) => format!("{prefix}{name}"),
                None => token.clone(),
            })
            .collect()
    }
}

/// Reason for a placeholder in a place the grammar does not allow.
const PLACEMENT: &str =
    "the {session_name} placeholder must end a `-<flag>=` token or follow a flag token";

/// Whether `token` is a flag name: `-` or `--`, then an alphanumeric, then
/// alphanumerics and dashes. That refuses `-`, `--`, `---x` and any token
/// holding `=`, a brace or another character.
fn is_flag(token: &str) -> bool {
    let name = token.strip_prefix("--").or_else(|| token.strip_prefix('-'));
    name.is_some_and(|name| {
        let mut characters = name.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
            && characters.all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn refuse(reason: &'static str) -> DefinitionError {
    DefinitionError::Field {
        field: FIELD,
        reason,
    }
}
