//! A runtime descriptor's `[placement]` table: how the runtime keeps a session
//! out of a shared host daemon.
//!
//! ```toml
//! requires = ["placement-v1"]
//!
//! [placement]
//! mode = "in_process"
//! keep_local_args = ["--no-daemon"]
//! ```
//!
//! A session's agent runs only inside processes its worker generation owns.
//! `mode = "in_process"` declares that the agent stays in the PTY's process
//! tree, in one of three ways:
//!
//! - `keep_local_args`: literal arguments, validated like `[runtime] args`,
//!   that the core adds to every create, resume and fork launch. They are
//!   never part of the frozen resume snapshot, and a launch whose frozen
//!   arguments already hold all of them gets no second copy.
//! - `keep_local_handler = true`: the runtime's compiled integration handler
//!   contributes the launch input that keeps the agent local.
//! - neither: the agent has no daemon to keep out.
//!
//! `mode = "owned_server"` is reserved for a backend this core does not
//! implement yet and is refused. A runtime that declares the table also gets
//! the worker's escape check.

// Rust guideline compliant 2026-11-06

use serde::Deserialize;

use super::definition::{validate_token, DefinitionError};
use super::naming::SESSION_NAME_PLACEHOLDER;

/// Maximum number of tokens in `[placement] keep_local_args`.
pub const MAX_KEEP_LOCAL_TOKENS: usize = 8;

/// Descriptor field the diagnostics of this module name.
const FIELD: &str = "placement.keep_local_args";

/// The validated `[placement]` declaration of a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    keep_local_args: Vec<String>,
    keep_local_handler: bool,
}

impl Placement {
    /// A placement that keeps the agent local by itself.
    #[must_use]
    pub fn nothing_to_add() -> Self {
        Self {
            keep_local_args: Vec::new(),
            keep_local_handler: false,
        }
    }

    /// A placement kept local by a literal argument list.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::Field`] for no token or more than
    /// [`MAX_KEEP_LOCAL_TOKENS`], an empty, oversized or control-character
    /// token, or the `{session_name}` placeholder.
    pub fn with_args(tokens: Vec<String>) -> Result<Self, DefinitionError> {
        if tokens.is_empty() || tokens.len() > MAX_KEEP_LOCAL_TOKENS {
            return Err(DefinitionError::Field {
                field: FIELD,
                reason: "must hold 1..=8 tokens",
            });
        }
        for token in &tokens {
            validate_token(token, FIELD)?;
            if token.contains(SESSION_NAME_PLACEHOLDER) {
                return Err(DefinitionError::Field {
                    field: FIELD,
                    reason: "must not hold the {session_name} placeholder",
                });
            }
        }
        Ok(Self {
            keep_local_args: tokens,
            keep_local_handler: false,
        })
    }

    /// A placement kept local by the runtime's integration handler.
    #[must_use]
    pub fn with_handler() -> Self {
        Self {
            keep_local_args: Vec::new(),
            keep_local_handler: true,
        }
    }

    /// The literal arguments, empty when the placement declares none.
    #[must_use]
    pub fn keep_local_args(&self) -> &[String] {
        &self.keep_local_args
    }

    /// Whether the integration handler keeps the agent local.
    #[must_use]
    pub fn keeps_local_by_handler(&self) -> bool {
        self.keep_local_handler
    }

    /// The arguments to add to a launch whose frozen arguments are
    /// `frozen_args`: the declared ones, or none when the frozen arguments
    /// already hold all of them.
    #[must_use]
    pub fn launch_args(&self, frozen_args: &[String]) -> Vec<String> {
        if self
            .keep_local_args
            .iter()
            .all(|token| frozen_args.contains(token))
        {
            Vec::new()
        } else {
            self.keep_local_args.clone()
        }
    }
}

/// How the runtime stays out of a shared host daemon.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RawPlacementMode {
    /// The agent stays in the PTY's process tree.
    InProcess,
    /// A server the worker owns; not implemented yet.
    OwnedServer,
}

/// The raw `[placement]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPlacement {
    mode: RawPlacementMode,
    #[serde(default)]
    keep_local_args: Option<Vec<String>>,
    #[serde(default)]
    keep_local_handler: bool,
}

impl RawPlacement {
    /// Validates the table.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::PlacementModeUnsupported`] for `owned_server`,
    /// [`DefinitionError::Field`] when both keep-local forms are declared, and
    /// the errors of [`Placement::with_args`].
    pub(super) fn validated(&self) -> Result<Placement, DefinitionError> {
        match self.mode {
            RawPlacementMode::OwnedServer => Err(DefinitionError::PlacementModeUnsupported),
            RawPlacementMode::InProcess => match (&self.keep_local_args, self.keep_local_handler) {
                (Some(_), true) => Err(DefinitionError::Field {
                    field: "placement",
                    reason: "keep_local_args and keep_local_handler are mutually exclusive",
                }),
                (Some(tokens), false) => Placement::with_args(tokens.clone()),
                (None, true) => Ok(Placement::with_handler()),
                (None, false) => Ok(Placement::nothing_to_add()),
            },
        }
    }
}
