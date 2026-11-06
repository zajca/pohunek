//! A runtime descriptor's `[runtime] trusted_checkout_args` and
//! `trusted_checkout_paths`: the arguments a launch gets only when its checkout
//! passes the host's trust predicate, and the checkout paths that predicate
//! compares with the default branch.
//!
//! ```toml
//! [runtime]
//! trusted_checkout_args = ["--trust-workspace"]
//! trusted_checkout_paths = [".muse", ".agents"]
//! ```
//!
//! The arguments are literal. The paths name what the runtime loads from a
//! checkout once it trusts it (hooks, rules, plugin configuration): each is
//! relative to the checkout root, with normalized `/`-separated components.

// Rust guideline compliant 2026-11-06

use serde::Serialize;

use super::definition::{validate_token, DefinitionError, MAX_ARG_BYTES};
use super::naming::SESSION_NAME_PLACEHOLDER;

/// Maximum number of tokens in `trusted_checkout_args`.
pub const MAX_CHECKOUT_TRUST_ARGS: usize = 8;

/// Maximum number of entries in `trusted_checkout_paths`.
pub const MAX_CHECKOUT_TRUST_PATHS: usize = 16;

/// Maximum size of one `trusted_checkout_paths` entry, in bytes.
pub const MAX_CHECKOUT_TRUST_PATH_BYTES: usize = 256;

/// The git metadata directory, which no checkout path may name.
const GIT_DIR_NAME: &str = ".git";

/// Descriptor field of the arguments, named in diagnostics.
const ARGS_FIELD: &str = "runtime.trusted_checkout_args";

/// Descriptor field of the paths, named in diagnostics.
const PATHS_FIELD: &str = "runtime.trusted_checkout_paths";

/// The validated trusted-checkout declaration of a runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckoutTrust {
    args: Vec<String>,
    paths: Vec<String>,
}

impl CheckoutTrust {
    /// Validates the declared arguments and paths.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::Field`] for no argument or more than
    /// [`MAX_CHECKOUT_TRUST_ARGS`], an invalid argument token or one holding
    /// the `{session_name}` placeholder, no path or more than
    /// [`MAX_CHECKOUT_TRUST_PATHS`], a path that is not relative and
    /// normalized, or a repeated path. A diagnostic
    /// never reflects an entry.
    pub fn new(args: Vec<String>, paths: Vec<String>) -> Result<Self, DefinitionError> {
        if args.is_empty() || args.len() > MAX_CHECKOUT_TRUST_ARGS {
            return Err(refuse(ARGS_FIELD, "must hold 1..=8 arguments"));
        }
        for arg in &args {
            validate_token(arg, ARGS_FIELD)?;
            if arg.contains(SESSION_NAME_PLACEHOLDER) {
                return Err(refuse(
                    ARGS_FIELD,
                    "must not hold the {session_name} placeholder",
                ));
            }
        }
        if paths.is_empty() || paths.len() > MAX_CHECKOUT_TRUST_PATHS {
            return Err(refuse(
                PATHS_FIELD,
                "must hold 1..=16 paths whenever trusted_checkout_args is set",
            ));
        }
        for (index, path) in paths.iter().enumerate() {
            validate_path(path)?;
            if paths[..index].contains(path) {
                return Err(refuse(PATHS_FIELD, "must not repeat a path"));
            }
        }
        Ok(Self { args, paths })
    }

    /// The arguments a trusted checkout adds to the launch.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The checkout paths the trust predicate compares and requires clean.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }
}

/// Checks one checkout path: 1..=[`MAX_CHECKOUT_TRUST_PATH_BYTES`] bytes of
/// `/`-separated components, none empty, `.`, `..` or `.git`, with no leading
/// or trailing `/`, backslash, `:` or control character. The path therefore
/// names one place below the checkout root and is safe as a git revision
/// suffix and as a pathspec.
fn validate_path(path: &str) -> Result<(), DefinitionError> {
    if path.is_empty() || path.len() > MAX_CHECKOUT_TRUST_PATH_BYTES.min(MAX_ARG_BYTES) {
        return Err(refuse(PATHS_FIELD, "has an empty or oversized path"));
    }
    if path
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | ':'))
    {
        return Err(refuse(
            PATHS_FIELD,
            "must not hold control characters, backslashes or colons",
        ));
    }
    let normalized = path.split('/').all(|part| {
        !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(GIT_DIR_NAME)
    });
    if !normalized {
        return Err(refuse(
            PATHS_FIELD,
            "must be a relative normalized path without `.`, `..`, `.git` or empty components",
        ));
    }
    Ok(())
}

fn refuse(field: &'static str, reason: &'static str) -> DefinitionError {
    DefinitionError::Field { field, reason }
}
