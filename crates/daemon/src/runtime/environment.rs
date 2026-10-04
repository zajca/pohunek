//! Selects the non-secret base environment the daemon hands to session workers.
//!
//! Workers build their agent child from an empty environment, so the daemon
//! decides which of its own session variables (`PATH`, `HOME`, `LANG`, ...) the
//! agent sees. The selection is an allowlist of exact names and trailing-`*`
//! prefixes; service-manager variables and `POHUNEK_*` identity markers never
//! pass, whatever the allowlist says.

// Rust guideline compliant 2026-09-24

use std::collections::BTreeMap;
use std::ffi::OsString;

use pohunek_worker_protocol::{BaseEnv, EnvError};

/// Where the daemon reads the variables it filters into a session's base
/// environment.
///
/// The allowlist, the denylist and the [`BaseEnv`] bounds apply to every source
/// alike; only the variables they are applied to differ.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnvironmentSource {
    /// The environment of this daemon process.
    #[default]
    Process,
    /// An explicit set of variables, independent of the process environment.
    ///
    /// A test supplies the environment of a hermetic fixture so the agent
    /// child sees the fixture's `HOME` and none of the developer's variables.
    Fixed(BTreeMap<OsString, OsString>),
}

impl EnvironmentSource {
    /// Builds an [`EnvironmentSource::Fixed`] from `variables`; a repeated name
    /// keeps its last value.
    pub fn fixed(variables: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self::Fixed(variables.into_iter().collect())
    }
}

/// Selects the variables of `source` that match `allowlist`.
///
/// `allowlist` is the configured list, or
/// [`pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST`] when none is
/// configured.
///
/// # Errors
///
/// Returns [`EnvError`] when a pattern is malformed or a selected variable
/// exceeds a [`BaseEnv`] bound.
pub(crate) fn base_environment<P: AsRef<str>>(
    allowlist: &[P],
    source: &EnvironmentSource,
) -> Result<BaseEnv, EnvError> {
    match source {
        EnvironmentSource::Process => BaseEnv::from_allowlist(allowlist, std::env::vars_os()),
        EnvironmentSource::Fixed(variables) => BaseEnv::from_allowlist(
            allowlist,
            variables
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        ),
    }
}

/// The value `name` has in the agent child's environment.
///
/// The child starts from `base`, then the profile environment overrides it (a
/// later profile entry wins); reserved `POHUNEK_*` names never come from the
/// profile. Recovery reads variables through this function so it sees what the
/// launched agent sees.
pub(crate) fn effective_variable(
    base: &BaseEnv,
    profile: &[(String, String)],
    name: &str,
) -> Option<OsString> {
    profile
        .iter()
        .rev()
        .find(|(key, _)| key == name && !key.starts_with("POHUNEK_"))
        .map(|(_, value)| OsString::from(value))
        .or_else(|| base.get(name).map(OsString::from))
}

#[cfg(test)]
mod tests {
    use pohunek_worker_protocol::{is_denylisted, DEFAULT_ENVIRONMENT_ALLOWLIST};

    use super::*;

    #[test]
    fn default_allowlist_reads_this_process_environment() {
        let base = base_environment(DEFAULT_ENVIRONMENT_ALLOWLIST, &EnvironmentSource::Process)
            .expect("default allowlist");

        assert_eq!(
            base.get("PATH"),
            std::env::var("PATH").ok().as_deref(),
            "PATH must be forwarded exactly when the daemon has one"
        );
        for (name, _) in base.iter() {
            assert!(!is_denylisted(name), "{name} must never be forwarded");
            assert!(
                !name.starts_with("POHUNEK_"),
                "{name} must never be forwarded"
            );
        }
    }

    #[test]
    fn a_narrow_allowlist_forwards_only_its_names() {
        let base =
            base_environment(&["PATH"], &EnvironmentSource::Process).expect("narrow allowlist");

        assert!(base.iter().all(|(name, _)| name == "PATH"));
    }

    #[test]
    fn a_malformed_allowlist_fails() {
        assert!(matches!(
            base_environment(&["*"], &EnvironmentSource::Process),
            Err(EnvError::InvalidPattern { .. })
        ));
    }

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    #[test]
    fn a_fixed_source_ignores_the_process_environment() {
        let source = EnvironmentSource::fixed([
            pair("HOME", "/fixture/home"),
            pair("DEVELOPER_ONLY_NAME", "kept-out"),
        ]);

        let base = base_environment(DEFAULT_ENVIRONMENT_ALLOWLIST, &source).expect("fixed source");

        assert_eq!(base.get("HOME"), Some("/fixture/home"));
        assert_eq!(
            base.iter().len(),
            1,
            "only the allowlisted name is selected"
        );
    }

    #[test]
    fn a_fixed_source_keeps_the_denylist_and_the_reserved_prefix() {
        let source = EnvironmentSource::fixed([
            pair("HOME", "/fixture/home"),
            pair("POHUNEK_SESSION_ID", "s-1"),
            pair("INVOCATION_ID", "unit"),
        ]);

        let base = base_environment(&["HOME"], &source).expect("fixed source");
        assert_eq!(base.iter().len(), 1);

        let wide = base_environment(&["POHUNEK_*", "INVOCATION_*", "HOME"], &source)
            .expect("wide allowlist");
        assert_eq!(wide.get("HOME"), Some("/fixture/home"));
        assert!(wide.get("POHUNEK_SESSION_ID").is_none());
        assert!(wide.get("INVOCATION_ID").is_none());
    }

    #[test]
    fn a_fixed_source_applies_the_same_pattern_validation() {
        let source = EnvironmentSource::fixed([pair("HOME", "/fixture/home")]);

        assert!(matches!(
            base_environment(&["*"], &source),
            Err(EnvError::InvalidPattern { .. })
        ));
    }

    #[test]
    fn a_profile_variable_overrides_the_base_and_the_base_fills_the_rest() {
        let source = EnvironmentSource::fixed([
            pair("HOME", "/fixture/home"),
            pair("XDG_CONFIG_HOME", "/fixture/xdg"),
            pair("NOT_ALLOWLISTED_NAME", "dropped"),
        ]);
        let base = base_environment(DEFAULT_ENVIRONMENT_ALLOWLIST, &source).expect("base");
        let profile = vec![
            ("XDG_CONFIG_HOME".to_owned(), "/profile/old".to_owned()),
            ("XDG_CONFIG_HOME".to_owned(), "/profile/xdg".to_owned()),
            ("POHUNEK_SESSION_ID".to_owned(), "spoofed".to_owned()),
        ];

        let get = |name| effective_variable(&base, &profile, name);
        assert_eq!(get("XDG_CONFIG_HOME"), Some(OsString::from("/profile/xdg")));
        assert_eq!(get("HOME"), Some(OsString::from("/fixture/home")));
        assert_eq!(get("NOT_ALLOWLISTED_NAME"), None);
        assert_eq!(get("POHUNEK_SESSION_ID"), None);
    }
}
