//! A runtime descriptor's `[launch_env]`: literal environment pairs every
//! launch and version probe of the runtime gets.
//!
//! ```toml
//! [launch_env]
//! ACME_NO_AUTO_UPDATE = "1"
//! ```
//!
//! The pairs are plain strings, never expanded. They are validated here and held
//! on the definition; nothing persists them, so each launch reads them from the
//! definition that serves it. A public descriptor must not blank or replace an
//! owner credential, the search path or a variable the core owns, so loader,
//! locale and XDG names, the core's own names and secret-shaped names are
//! refused, as are service-manager names and the variables the descriptor
//! reads its own config home from. The trust model stays same-UID (see "Runtime packages and trust"):
//! this guards against mistakes and shadowing, it is not a sandbox.

// Rust guideline compliant 2026-10-27

use std::collections::BTreeMap;

use pohunek_worker_protocol::is_denylisted;

use super::definition::DefinitionError;
use crate::agent::env_name_problem;

/// Maximum number of `[launch_env]` entries.
///
/// A launch variable switches one behavior of the agent; more entries are a
/// descriptor that configures the agent wholesale.
pub const MAX_LAUNCH_ENV_ENTRIES: usize = 16;

/// Maximum size of one `[launch_env]` value, in bytes.
///
/// Values are flags, small identifiers or short paths.
pub const MAX_LAUNCH_ENV_VALUE_BYTES: usize = 256;

/// Name prefixes a descriptor cannot set: the dynamic loader, the locale and
/// the XDG base directories, which the probe sandbox and the launch own.
const REFUSED_PREFIXES: &[&str] = &["LD_", "DYLD_", "XDG_", "LC_"];

/// Names a descriptor cannot set: the search path, the user identity, the
/// terminal and locale the launch fixes, the private `TMPDIR`, and the hook
/// path the integration contributes.
const REFUSED_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "SHELL",
    "USER",
    "LOGNAME",
    "TERM",
    "COLORTERM",
    "LANG",
    "TBH_MANAGED_HOOKS_PATH",
];

/// Suffixes of secret-shaped names, which only the owner's profile sets.
const SECRET_SUFFIXES: &[&str] = &[
    "_KEY",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_PASSWD",
    "_CREDENTIALS",
    "_AUTH",
];

/// The validated `[launch_env]` pairs of a definition, ordered by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorLaunchEnv(Vec<(String, String)>);

impl DescriptorLaunchEnv {
    /// Validates the declared pairs.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming `launch_env`, never a name
    /// or a value, for more than [`MAX_LAUNCH_ENV_ENTRIES`] entries, a name
    /// that is not an upper-case variable outside the `POHUNEK_` namespace, a
    /// refused or secret-shaped name, or a value over
    /// [`MAX_LAUNCH_ENV_VALUE_BYTES`] or holding a control character.
    pub fn new(declared: BTreeMap<String, String>) -> Result<Self, DefinitionError> {
        let field = |reason| DefinitionError::Field {
            field: "launch_env",
            reason,
        };
        if declared.len() > MAX_LAUNCH_ENV_ENTRIES {
            return Err(field("has too many entries"));
        }
        for (name, value) in &declared {
            if let Some(reason) = env_name_problem(name) {
                return Err(field(reason));
            }
            if REFUSED_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
                || REFUSED_NAMES.contains(&name.as_str())
            {
                return Err(field("cannot name a variable the core or the loader owns"));
            }
            if is_denylisted(name) {
                return Err(field("cannot name a service-manager variable"));
            }
            if SECRET_SUFFIXES
                .iter()
                .any(|suffix| name.ends_with(suffix) || *name == suffix[1..])
            {
                return Err(field("cannot name a secret-shaped variable"));
            }
            if value.len() > MAX_LAUNCH_ENV_VALUE_BYTES {
                return Err(field("holds a value over the size bound"));
            }
            if value.chars().any(char::is_control) {
                return Err(field("holds a value with a control character"));
            }
        }
        Ok(Self(declared.into_iter().collect()))
    }

    /// Refuses a pair that names one of `owned`, the variables the same
    /// descriptor reads its config home or conversation root from: the launch
    /// would give the agent a home the daemon's own resolution never sees.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming `launch_env`.
    pub fn refuse_owned<'a>(
        &self,
        owned: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), DefinitionError> {
        let owned: Vec<&str> = owned.into_iter().collect();
        if self
            .0
            .iter()
            .any(|(name, _)| owned.contains(&name.as_str()))
        {
            return Err(DefinitionError::Field {
                field: "launch_env",
                reason: "cannot name a config home or conversation root variable",
            });
        }
        Ok(())
    }

    /// The pairs in name order.
    #[must_use]
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0
    }

    /// Whether the descriptor declares no pair.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
