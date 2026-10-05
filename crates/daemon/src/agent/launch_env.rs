//! The environment variables a launch adds to the agent's base environment.
//!
//! Profile `[env]` entries are secret-bearing by design (API keys, config-home
//! paths), so the carrier never prints its keys or values: every `Debug`
//! rendering of a type that embeds it shows only the entry count. The values
//! reach a child only through [`LaunchEnv::as_slice`].

// Rust guideline compliant 2026-10-05

use std::fmt;

/// Ordered `(name, value)` environment entries handed to a launched agent.
///
/// Later entries win over earlier ones for the same name when the launch
/// applies them in order.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LaunchEnv(Vec<(String, String)>);

impl LaunchEnv {
    /// Wraps `entries`, keeping their order.
    #[must_use]
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self(entries)
    }

    /// The entries in launch order.
    #[must_use]
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0
    }
}

impl std::ops::Deref for LaunchEnv {
    type Target = [(String, String)];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for LaunchEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LaunchEnv({} entries, redacted)", self.0.len())
    }
}

impl From<Vec<(String, String)>> for LaunchEnv {
    fn from(entries: Vec<(String, String)>) -> Self {
        Self(entries)
    }
}

impl Extend<(String, String)> for LaunchEnv {
    fn extend<I: IntoIterator<Item = (String, String)>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl PartialEq<[(String, String)]> for LaunchEnv {
    fn eq(&self, other: &[(String, String)]) -> bool {
        self.0 == other
    }
}

impl<const N: usize> PartialEq<[(String, String); N]> for LaunchEnv {
    fn eq(&self, other: &[(String, String); N]) -> bool {
        self.0 == other.as_slice()
    }
}

impl PartialEq<Vec<(String, String)>> for LaunchEnv {
    fn eq(&self, other: &Vec<(String, String)>) -> bool {
        &self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::LaunchEnv;
    use crate::agent::{LaunchCommand, LaunchOpts};

    const SECRET_NAME: &str = "ANTHROPIC_API_KEY";
    const SECRET_VALUE: &str = "sk-ant-never-print-this";

    fn secret_env() -> LaunchEnv {
        LaunchEnv::new(vec![(SECRET_NAME.to_owned(), SECRET_VALUE.to_owned())])
    }

    fn assert_redacted(rendered: &str) {
        assert!(!rendered.contains(SECRET_VALUE), "value leaked: {rendered}");
        assert!(!rendered.contains(SECRET_NAME), "name leaked: {rendered}");
    }

    #[test]
    fn debug_of_the_env_shows_neither_names_nor_values() {
        let env = secret_env();
        assert_redacted(&format!("{env:?}"));
        assert_redacted(&format!("{env:#?}"));
        assert!(format!("{env:?}").contains("1 entries"));
    }

    #[test]
    fn debug_of_the_launch_carriers_shows_neither_names_nor_values() {
        let opts = LaunchOpts {
            cwd: std::path::PathBuf::from("/work"),
            cols: 80,
            rows: 24,
            env_extra: secret_env(),
            validated_program: None,
        };
        assert_redacted(&format!("{opts:?}"));
        assert_redacted(&format!("{opts:#?}"));

        let command = LaunchCommand {
            program: "agent".to_owned(),
            args: Vec::new(),
            env: secret_env(),
            cwd: std::path::PathBuf::from("/work"),
            cols: 80,
            rows: 24,
        };
        assert_redacted(&format!("{command:?}"));
        assert_redacted(&format!("{command:#?}"));
    }

    #[test]
    fn extend_appends_after_existing_entries() {
        let mut env = secret_env();
        env.extend([("LATER".to_owned(), "value".to_owned())]);
        assert_eq!(env.as_slice().len(), 2);
        assert_eq!(env.as_slice()[1].0, "LATER");
    }
}
