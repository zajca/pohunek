//! Selects the non-secret base environment the daemon hands to session workers.
//!
//! Workers build their agent child from an empty environment, so the daemon
//! decides which of its own session variables (`PATH`, `HOME`, `LANG`, ...) the
//! agent sees. The selection is an allowlist of exact names and trailing-`*`
//! prefixes; service-manager variables and `POHUNEK_*` identity markers never
//! pass, whatever the allowlist says.

// Rust guideline compliant 2026-09-24

use pohunek_worker_protocol::{BaseEnv, EnvError};

/// Selects the variables of this daemon process that match `allowlist`.
///
/// `allowlist` is the configured list, or
/// [`pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST`] when none is
/// configured.
///
/// # Errors
///
/// Returns [`EnvError`] when a pattern is malformed or a selected variable
/// exceeds a [`BaseEnv`] bound.
pub(crate) fn base_environment<P: AsRef<str>>(allowlist: &[P]) -> Result<BaseEnv, EnvError> {
    BaseEnv::from_allowlist(allowlist, std::env::vars_os())
}

#[cfg(test)]
mod tests {
    use pohunek_worker_protocol::{is_denylisted, DEFAULT_ENVIRONMENT_ALLOWLIST};

    use super::*;

    #[test]
    fn default_allowlist_reads_this_process_environment() {
        let base = base_environment(DEFAULT_ENVIRONMENT_ALLOWLIST).expect("default allowlist");

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
        let base = base_environment(&["PATH"]).expect("narrow allowlist");

        assert!(base.iter().all(|(name, _)| name == "PATH"));
    }

    #[test]
    fn a_malformed_allowlist_fails() {
        assert!(matches!(
            base_environment(&["*"]),
            Err(EnvError::InvalidPattern { .. })
        ));
    }
}
