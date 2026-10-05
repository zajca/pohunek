//! A runtime's config home: the directory its agent keeps its settings, hook
//! registration and conversations in.
//!
//! A descriptor declares the home in an optional `[config_home]` table, apart
//! from `[integration]` because the home concerns every runtime: the hook
//! integration installs into it, and observation and recovery read from it.
//!
//! ```toml
//! [config_home]
//! env = "CLAUDE_CONFIG_DIR"
//! default = ".claude"
//! ```
//!
//! `env` names the variable the agent reads to relocate its home and `default`
//! is the directory below the user's home directory that applies while the
//! variable is unset or empty. The table only names where to look; the daemon
//! decides what it does there.

// Rust guideline compliant 2026-10-05

use std::ffi::OsString;
use std::path::PathBuf;

use super::definition::DefinitionError;
use crate::agent::{env_name_problem, is_plain_relative_path};

/// Environment variable that names the user's home directory.
const HOME_ENV: &str = "HOME";

/// A validated `[config_home]` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigHome {
    env: String,
    default: String,
}

impl ConfigHome {
    /// Validates the declared variable name and home-relative default.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when `env` is not an upper-case
    /// variable name outside the reserved `POHUNEK_` namespace, or `default`
    /// is not a bounded relative path of plain components (no absolute path,
    /// no `..`, no glob or shell metacharacter).
    pub fn new(env: &str, default: &str) -> Result<Self, DefinitionError> {
        if let Some(reason) = env_name_problem(env) {
            return Err(DefinitionError::Field {
                field: "config_home.env",
                reason,
            });
        }
        if !is_plain_relative_path(default) {
            return Err(DefinitionError::Field {
                field: "config_home.default",
                reason: "must be a relative path of plain components",
            });
        }
        Ok(Self {
            env: env.to_owned(),
            default: default.to_owned(),
        })
    }

    /// The variable the agent reads to relocate its home.
    #[must_use]
    pub fn env(&self) -> &str {
        &self.env
    }

    /// The home directory below the user's home directory used while the
    /// variable is unset or empty.
    #[must_use]
    pub fn default_relative(&self) -> &str {
        &self.default
    }

    /// The config home a launch would give the agent: the declared variable
    /// when `lookup` finds it non-empty, else the declared default below the
    /// `HOME` `lookup` finds.
    ///
    /// `lookup` is the environment the launched agent sees, so the answer is
    /// the agent's own. Neither value is expanded: a `~` is a relative path
    /// and is refused, never resolved against the daemon's home directory.
    ///
    /// # Errors
    ///
    /// [`HomeError::Unset`] when the variable and `HOME` are both unset, and
    /// [`HomeError::Invalid`] when the chosen value is not an absolute UTF-8
    /// path.
    pub fn resolve(&self, lookup: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, HomeError> {
        let (path, source) =
            if let Some(value) = lookup(&self.env).filter(|value| !value.is_empty()) {
                (PathBuf::from(value), self.env.as_str())
            } else {
                let home = lookup(HOME_ENV)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| HomeError::Unset {
                        env: self.env.clone(),
                    })?;
                (PathBuf::from(home).join(&self.default), HOME_ENV)
            };
        if path.is_absolute() && path.to_str().is_some() {
            Ok(path)
        } else {
            Err(HomeError::Invalid {
                variable: source.to_owned(),
            })
        }
    }
}

/// Why a config home cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HomeError {
    /// Neither the declared variable nor `HOME` is set.
    #[error("cannot resolve agent config dir: neither {env} nor HOME is set")]
    Unset {
        /// The declared variable.
        env: String,
    },
    /// The chosen value is relative or not UTF-8. The value is never named: it
    /// may come from a profile's secret-bearing environment.
    #[error("{variable} must resolve to an absolute UTF-8 path for agent hook registration")]
    Invalid {
        /// The variable that supplied the value (`HOME` for the default).
        variable: String,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn home() -> ConfigHome {
        ConfigHome::new("AGENT_HOME", ".agent").expect("valid declaration")
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: BTreeMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_declared_variable_wins_over_the_default() {
        let lookup = env(&[("AGENT_HOME", "/work/agent"), ("HOME", "/home/u")]);
        assert_eq!(home().resolve(&lookup), Ok(PathBuf::from("/work/agent")));
    }

    #[test]
    fn an_unset_or_empty_variable_falls_back_to_the_default_below_home() {
        for pairs in [
            vec![("HOME", "/home/u")],
            vec![("AGENT_HOME", ""), ("HOME", "/home/u")],
        ] {
            assert_eq!(
                home().resolve(&env(&pairs)),
                Ok(PathBuf::from("/home/u/.agent"))
            );
        }
    }

    #[test]
    fn no_variable_and_no_home_is_unset() {
        assert_eq!(
            home().resolve(&env(&[])),
            Err(HomeError::Unset {
                env: "AGENT_HOME".to_owned()
            })
        );
        assert_eq!(
            home().resolve(&env(&[("HOME", "")])),
            Err(HomeError::Unset {
                env: "AGENT_HOME".to_owned()
            })
        );
    }

    #[test]
    fn a_relative_value_is_refused_and_never_expanded() {
        for value in ["~", "~/agent", "relative/agent", "./agent"] {
            let lookup = env(&[("AGENT_HOME", value), ("HOME", "/home/u")]);
            assert_eq!(
                home().resolve(&lookup),
                Err(HomeError::Invalid {
                    variable: "AGENT_HOME".to_owned()
                }),
                "{value}"
            );
        }
    }

    #[test]
    fn a_relative_home_is_refused_as_the_home_source() {
        let lookup = env(&[("HOME", "relative")]);
        assert_eq!(
            home().resolve(&lookup),
            Err(HomeError::Invalid {
                variable: "HOME".to_owned()
            })
        );
    }

    #[test]
    fn the_error_never_names_the_refused_value() {
        let secret = "~/secret-profile-dir";
        let lookup = env(&[("AGENT_HOME", secret)]);
        let error = home().resolve(&lookup).expect_err("refused");
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn the_declaration_rejects_unsafe_names_and_paths() {
        for env in ["", "lower", "9START", "HAS-DASH", "POHUNEK_HOME"] {
            assert!(ConfigHome::new(env, ".agent").is_err(), "env {env:?}");
        }
        for default in [
            "", "/abs", "..", "../up", "a/../b", "~/x", "a b", "a/*", "a//b",
        ] {
            assert!(
                ConfigHome::new("AGENT_HOME", default).is_err(),
                "default {default:?}"
            );
        }
        ConfigHome::new("AGENT_HOME", ".config/agent").expect("a nested relative default");
    }

    #[test]
    fn the_declaration_bounds_the_default() {
        let too_long = "a".repeat(crate::agent::MAX_EXISTENCE_TEXT_BYTES + 1);
        ConfigHome::new("AGENT_HOME", &too_long).expect_err("a default over the byte bound");
        let deep = ["a"; crate::agent::MAX_EXISTENCE_PATH_COMPONENTS + 1].join("/");
        ConfigHome::new("AGENT_HOME", &deep).expect_err("a default over the depth bound");
    }
}
