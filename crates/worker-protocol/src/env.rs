//! Non-secret base environment handed from the daemon to a session worker.
//!
//! A worker started by a service manager (systemd or launchd) carries that
//! manager's environment, which describes the worker's own supervision rather
//! than the user's session. The worker therefore never forwards its own
//! environment to the agent child. Instead the daemon selects a small set of
//! session variables from its own environment through an allowlist and sends
//! them as a [`BaseEnv`] in `Initialize`. The worker builds the child
//! environment from an empty base plus that map, its own authoritative
//! `POHUNEK_*` identity, and the profile's secret environment, and then removes
//! every [`SERVICE_MANAGER_DENYLIST`] entry unconditionally.
//!
//! Allowlist and denylist entries share one pattern grammar: an exact variable
//! name, or a name prefix followed by a single trailing `*` (`LC_*`). Matching
//! is case-sensitive, as Unix environments are.
//!
//! [`BaseEnv`] values are not secret, so its `Debug` output lists variable
//! names. Values are still withheld because paths such as `HOME` identify the
//! user and do not help diagnose protocol behavior.

// Rust guideline compliant 2026-09-24

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{Debug, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Maximum number of variables in one [`BaseEnv`].
///
/// A login session exports a few dozen variables at most. The bound keeps a
/// misconfigured `*`-heavy allowlist from shipping an arbitrarily large map.
pub const MAX_BASE_ENV_ENTRIES: usize = 256;

/// Maximum bytes in one [`BaseEnv`] variable name.
pub const MAX_BASE_ENV_NAME_BYTES: usize = 128;

/// Maximum bytes in one [`BaseEnv`] variable value.
///
/// Long `PATH` values are the largest legitimate entries and stay far below
/// this bound.
pub const MAX_BASE_ENV_VALUE_BYTES: usize = 32 * 1024;

/// Maximum combined name and value bytes in one [`BaseEnv`].
///
/// `Initialize` travels as one control line together with the profile
/// environment and launch arguments, and the session worker reads control
/// lines of at most 64 KiB by default. The bound admits one maximal value plus
/// a few ordinary variables and leaves the rest of that line to the other
/// fields, so the base environment is never why initialization cannot decode.
pub const MAX_BASE_ENV_BYTES: usize = 40 * 1024;

/// Allowlist used when the service configuration does not narrow it.
///
/// These describe the user's login session. `TERM` is absent because the
/// worker sets it for its own PTY; terminal-emulator variables such as
/// `TERM_PROGRAM` and `COLORTERM` describe whatever launched the daemon, not
/// the session terminal, and are therefore excluded as well.
pub const DEFAULT_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_*",
    "TMPDIR",
    "SSH_AUTH_SOCK",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_*",
];

/// Variables a worker always removes from the child environment.
///
/// systemd (`NOTIFY_SOCKET`, `WATCHDOG_*`, `INVOCATION_ID`, `JOURNAL_STREAM`,
/// `MANAGERPID`, `SYSTEMD_EXEC_PID`) and launchd (`XPC_SERVICE_NAME`,
/// `XPC_FLAGS`, `__CFBundleIdentifier`, `LaunchInstanceID`) set these for the
/// supervised worker itself. An agent inheriting them could notify or be
/// accounted to the worker's unit. The two tokens authenticate the worker to
/// the daemon and must never reach agent code. Removal applies even when a
/// variable is allowlisted or supplied by the profile.
pub const SERVICE_MANAGER_DENYLIST: &[&str] = &[
    "NOTIFY_SOCKET",
    "WATCHDOG_*",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "MANAGERPID",
    "SYSTEMD_EXEC_PID",
    "XPC_SERVICE_NAME",
    "XPC_FLAGS",
    "__CFBundleIdentifier",
    "LaunchInstanceID",
    "POHUNEK_CONTROLLER_TOKEN",
    "POHUNEK_BOOTSTRAP_TOKEN",
];

/// Name prefix owned by the worker's authoritative identity variables.
///
/// The worker sets every `POHUNEK_*` child variable itself, so a base
/// environment may never carry one; a daemon running inside another pohunek
/// session would otherwise leak its ancestor's identity to the agent.
const RESERVED_PREFIX: &str = "POHUNEK_";

/// Trailing marker that turns a pattern into a prefix match.
const PREFIX_WILDCARD: char = '*';

/// Reports an invalid base environment or environment pattern.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvError {
    /// A variable name is empty, too long, or outside `[A-Za-z_][A-Za-z0-9_]*`.
    #[error("environment variable name `{name}` is invalid")]
    InvalidName {
        /// Rejected name, truncated to the name bound.
        name: String,
    },
    /// A variable value exceeds [`MAX_BASE_ENV_VALUE_BYTES`].
    #[error("environment variable `{name}` value is {actual} bytes; maximum is {maximum}")]
    ValueTooLong {
        /// Variable name.
        name: String,
        /// Observed value length.
        actual: usize,
        /// Maximum accepted value length.
        maximum: usize,
    },
    /// A variable value contains a NUL byte and cannot reach `execve`.
    #[error("environment variable `{name}` value contains a NUL byte")]
    ValueContainsNul {
        /// Variable name.
        name: String,
    },
    /// A variable is on the [`SERVICE_MANAGER_DENYLIST`].
    #[error("environment variable `{name}` belongs to the service manager")]
    Denylisted {
        /// Variable name.
        name: String,
    },
    /// A variable uses the worker-owned `POHUNEK_` prefix.
    #[error("environment variable `{name}` uses the reserved POHUNEK_ prefix")]
    Reserved {
        /// Variable name.
        name: String,
    },
    /// The map holds more than [`MAX_BASE_ENV_ENTRIES`] variables.
    #[error("base environment has {actual} variables; maximum is {maximum}")]
    TooManyEntries {
        /// Observed entry count.
        actual: usize,
        /// Maximum accepted entry count.
        maximum: usize,
    },
    /// The map exceeds [`MAX_BASE_ENV_BYTES`] in combined size.
    #[error("base environment is {actual} bytes; maximum is {maximum}")]
    TooLarge {
        /// Observed combined name and value bytes.
        actual: usize,
        /// Maximum accepted combined bytes.
        maximum: usize,
    },
    /// An allowlist or denylist pattern is malformed.
    #[error("environment pattern `{pattern}` is invalid: {reason}")]
    InvalidPattern {
        /// Rejected pattern, truncated to the name bound.
        pattern: String,
        /// Why the pattern was rejected.
        reason: &'static str,
    },
}

/// Validated, non-secret environment the worker places under the child.
///
/// Every name matches `[A-Za-z_][A-Za-z0-9_]*` and is at most
/// [`MAX_BASE_ENV_NAME_BYTES`] long, every value is NUL-free and at most
/// [`MAX_BASE_ENV_VALUE_BYTES`] long, and the map never holds a denylisted or
/// `POHUNEK_*` variable. Deserialization re-applies these rules.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BaseEnv(BTreeMap<String, String>);

impl BaseEnv {
    /// Creates a validated base environment.
    ///
    /// # Errors
    ///
    /// Returns [`EnvError`] when a name is malformed, denylisted, or reserved,
    /// when a value is too long or contains NUL, or when the map exceeds its
    /// entry or byte bounds.
    pub fn new(values: BTreeMap<String, String>) -> Result<Self, EnvError> {
        if values.len() > MAX_BASE_ENV_ENTRIES {
            return Err(EnvError::TooManyEntries {
                actual: values.len(),
                maximum: MAX_BASE_ENV_ENTRIES,
            });
        }
        let mut total = 0_usize;
        for (name, value) in &values {
            validate_entry(name, value)?;
            total = total.saturating_add(name.len()).saturating_add(value.len());
        }
        if total > MAX_BASE_ENV_BYTES {
            return Err(EnvError::TooLarge {
                actual: total,
                maximum: MAX_BASE_ENV_BYTES,
            });
        }
        Ok(Self(values))
    }

    /// Selects the variables of `vars` that match an allowlist pattern.
    ///
    /// `patterns` are exact names or trailing-`*` prefixes, such as
    /// [`DEFAULT_ENVIRONMENT_ALLOWLIST`]. Variables whose name or value is not
    /// UTF-8, that are denylisted, or that use the reserved `POHUNEK_` prefix
    /// are skipped, because the worker would strip or override them anyway.
    /// Typically `vars` is [`std::env::vars_os`].
    ///
    /// # Errors
    ///
    /// Returns [`EnvError::InvalidPattern`] for a malformed pattern, and the
    /// other [`EnvError`] variants when a selected variable violates a
    /// [`BaseEnv`] bound, so an oversized allowlisted value fails loudly
    /// instead of disappearing from the session.
    pub fn from_allowlist<P: AsRef<str>>(
        patterns: &[P],
        vars: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, EnvError> {
        for pattern in patterns {
            validate_pattern(pattern.as_ref())?;
        }
        let mut selected = BTreeMap::new();
        for (name, value) in vars {
            let (Ok(name), Ok(value)) = (name.into_string(), value.into_string()) else {
                continue;
            };
            if is_denylisted(&name) || name.starts_with(RESERVED_PREFIX) {
                continue;
            }
            if patterns
                .iter()
                .any(|pattern| pattern_matches(pattern.as_ref(), &name))
            {
                selected.insert(name, value);
            }
        }
        Self::new(selected)
    }

    /// Borrows the variables in name order for child construction.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Returns the value of `name`, when present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Consumes the wrapper and returns the variables.
    #[must_use]
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }

    /// Returns the number of variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether the environment has no variables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Debug for BaseEnv {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseEnv")
            .field("names", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl<'de> Deserialize<'de> for BaseEnv {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::new(values).map_err(serde::de::Error::custom)
    }
}

/// Validates one allowlist or denylist pattern.
///
/// A pattern is a variable name, or a nonempty name prefix followed by one
/// trailing `*`. A bare `*` is rejected because it would forward everything.
///
/// # Errors
///
/// Returns [`EnvError::InvalidPattern`] describing the violated rule.
pub fn validate_pattern(pattern: &str) -> Result<(), EnvError> {
    let invalid = |reason| EnvError::InvalidPattern {
        pattern: truncate(pattern),
        reason,
    };
    let stem = pattern.strip_suffix(PREFIX_WILDCARD).unwrap_or(pattern);
    if stem.is_empty() {
        return Err(invalid("a pattern needs a name or a nonempty prefix"));
    }
    if stem.contains(PREFIX_WILDCARD) {
        return Err(invalid("`*` is only allowed once, at the end"));
    }
    if !valid_name(stem) {
        return Err(invalid(
            "names use [A-Za-z_][A-Za-z0-9_]* and at most 128 bytes",
        ));
    }
    Ok(())
}

/// Reports whether `name` is on the [`SERVICE_MANAGER_DENYLIST`].
#[must_use]
pub fn is_denylisted(name: &str) -> bool {
    SERVICE_MANAGER_DENYLIST
        .iter()
        .any(|pattern| pattern_matches(pattern, name))
}

/// Matches `name` against an already validated pattern.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix(PREFIX_WILDCARD) {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

fn validate_entry(name: &str, value: &str) -> Result<(), EnvError> {
    if !valid_name(name) {
        return Err(EnvError::InvalidName {
            name: truncate(name),
        });
    }
    if is_denylisted(name) {
        return Err(EnvError::Denylisted {
            name: name.to_owned(),
        });
    }
    if name.starts_with(RESERVED_PREFIX) {
        return Err(EnvError::Reserved {
            name: name.to_owned(),
        });
    }
    if value.len() > MAX_BASE_ENV_VALUE_BYTES {
        return Err(EnvError::ValueTooLong {
            name: name.to_owned(),
            actual: value.len(),
            maximum: MAX_BASE_ENV_VALUE_BYTES,
        });
    }
    if value.as_bytes().contains(&0) {
        return Err(EnvError::ValueContainsNul {
            name: name.to_owned(),
        });
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_BASE_ENV_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Bounds untrusted names echoed in errors to the name length limit.
fn truncate(value: &str) -> String {
    value
        .char_indices()
        .take_while(|(index, _)| *index < MAX_BASE_ENV_NAME_BYTES)
        .map(|(_, character)| character)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn env(pairs: &[(&str, &str)]) -> Result<BaseEnv, EnvError> {
        BaseEnv::new(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn debug_lists_names_but_never_values() {
        let value = "/home/value-that-must-not-leak";
        let base = env(&[("HOME", value), ("LANG", "C.UTF-8")]).expect("valid base");
        let rendered = format!("{base:?}");

        assert!(rendered.contains("HOME"));
        assert!(rendered.contains("LANG"));
        assert!(!rendered.contains(value));
        assert!(!rendered.contains("C.UTF-8"));
    }

    #[test]
    fn names_follow_the_portable_grammar() {
        for name in ["PATH", "_x", "LC_ALL", "a1", "__CFBundleIdentifierLike"] {
            env(&[(name, "v")]).expect("valid name");
        }
        for name in ["", "1PATH", "A-B", "A=B", "A B", "Ä", "A\0B"] {
            assert!(
                matches!(env(&[(name, "v")]), Err(EnvError::InvalidName { .. })),
                "{name:?} must be rejected"
            );
        }
        let long = "A".repeat(MAX_BASE_ENV_NAME_BYTES + 1);
        assert!(matches!(
            env(&[(long.as_str(), "v")]),
            Err(EnvError::InvalidName { name }) if name.len() == MAX_BASE_ENV_NAME_BYTES
        ));
        env(&[("A".repeat(MAX_BASE_ENV_NAME_BYTES).as_str(), "v")]).expect("name at the bound");
    }

    #[test]
    fn values_are_bounded_and_nul_free() {
        let at_bound = "v".repeat(MAX_BASE_ENV_VALUE_BYTES);
        env(&[("A", at_bound.as_str())]).expect("value at the bound");
        let over = "v".repeat(MAX_BASE_ENV_VALUE_BYTES + 1);
        assert!(matches!(
            env(&[("A", over.as_str())]),
            Err(EnvError::ValueTooLong { actual, .. }) if actual == MAX_BASE_ENV_VALUE_BYTES + 1
        ));
        assert_eq!(
            env(&[("A", "x\0y")]),
            Err(EnvError::ValueContainsNul {
                name: "A".to_owned()
            })
        );
    }

    #[test]
    fn entry_count_and_total_size_are_bounded() {
        let at_bound = (0..MAX_BASE_ENV_ENTRIES)
            .map(|index| (format!("V{index}"), String::new()))
            .collect();
        BaseEnv::new(at_bound).expect("entry count at the bound");
        let over = (0..=MAX_BASE_ENV_ENTRIES)
            .map(|index| (format!("V{index}"), String::new()))
            .collect();
        assert!(matches!(
            BaseEnv::new(over),
            Err(EnvError::TooManyEntries { actual, .. }) if actual == MAX_BASE_ENV_ENTRIES + 1
        ));

        let half = "v".repeat(MAX_BASE_ENV_BYTES / 2);
        let too_large = BTreeMap::from([("A".to_owned(), half.clone()), ("B".to_owned(), half)]);
        assert!(matches!(
            BaseEnv::new(too_large),
            Err(EnvError::TooLarge { actual, .. }) if actual == MAX_BASE_ENV_BYTES + 2
        ));
    }

    #[test]
    fn denylisted_and_reserved_names_are_rejected() {
        for name in [
            "NOTIFY_SOCKET",
            "WATCHDOG_USEC",
            "WATCHDOG_PID",
            "INVOCATION_ID",
            "JOURNAL_STREAM",
            "MANAGERPID",
            "SYSTEMD_EXEC_PID",
            "XPC_SERVICE_NAME",
            "XPC_FLAGS",
            "__CFBundleIdentifier",
            "LaunchInstanceID",
            "POHUNEK_CONTROLLER_TOKEN",
            "POHUNEK_BOOTSTRAP_TOKEN",
        ] {
            assert!(is_denylisted(name), "{name} must be denylisted");
            assert!(
                matches!(env(&[(name, "v")]), Err(EnvError::Denylisted { .. })),
                "{name} must be rejected"
            );
        }
        assert!(matches!(
            env(&[("POHUNEK_SESSION_ID", "s-1")]),
            Err(EnvError::Reserved { .. })
        ));
        assert!(!is_denylisted("WATCHDOG"));
        assert!(!is_denylisted("notify_socket"));
    }

    #[test]
    fn deserialization_revalidates() {
        let error = serde_json::from_str::<BaseEnv>(r#"{"NOTIFY_SOCKET":"/run/notify"}"#)
            .expect_err("denylisted name must fail");
        assert!(error.to_string().contains("service manager"));

        let base: BaseEnv =
            serde_json::from_str(r#"{"PATH":"/usr/bin"}"#).expect("valid base environment");
        assert_eq!(base.get("PATH"), Some("/usr/bin"));
        assert_eq!(
            serde_json::to_string(&base).expect("serialize"),
            r#"{"PATH":"/usr/bin"}"#
        );
    }

    #[test]
    fn allowlist_selects_exact_names_and_prefixes_only() {
        let base = BaseEnv::from_allowlist(
            DEFAULT_ENVIRONMENT_ALLOWLIST,
            vars(&[
                ("PATH", "/usr/bin:/bin"),
                ("PATHS", "not-exact"),
                ("LC_ALL", "C"),
                ("LC_CTYPE", "C.UTF-8"),
                ("LC", "no-underscore"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("TERM", "xterm"),
                ("TERM_PROGRAM", "iTerm.app"),
                ("COLORTERM", "truecolor"),
                ("AWS_SECRET_ACCESS_KEY", "unrelated"),
                ("path", "lowercase"),
            ]),
        )
        .expect("valid allowlist");

        assert_eq!(
            base.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            ["LC_ALL", "LC_CTYPE", "PATH", "XDG_RUNTIME_DIR"]
        );
        assert_eq!(base.get("PATH"), Some("/usr/bin:/bin"));
    }

    #[test]
    fn allowlist_skips_denylisted_reserved_and_non_utf8_variables() {
        use std::os::unix::ffi::OsStringExt as _;

        let non_utf8 = OsString::from_vec(vec![0xff, 0xfe]);
        let mut input = vars(&[
            ("NOTIFY_SOCKET", "/run/notify"),
            ("WATCHDOG_USEC", "1"),
            ("POHUNEK_DAEMON_ID", "ancestor"),
            ("HOME", "/home/u"),
        ]);
        input.push((OsString::from("LANG"), non_utf8.clone()));
        input.push((non_utf8, OsString::from("value")));
        let patterns = [
            "NOTIFY_SOCKET".to_owned(),
            "WATCHDOG_*".to_owned(),
            "POHUNEK_*".to_owned(),
            "HOME".to_owned(),
            "LANG".to_owned(),
        ];

        let base = BaseEnv::from_allowlist(&patterns, input).expect("valid allowlist");

        assert_eq!(base.iter().collect::<Vec<_>>(), [("HOME", "/home/u")]);
    }

    #[test]
    fn allowlist_fails_loudly_on_an_oversized_selected_value() {
        let over = "v".repeat(MAX_BASE_ENV_VALUE_BYTES + 1);
        assert!(matches!(
            BaseEnv::from_allowlist(&["PATH"], vars(&[("PATH", over.as_str())])),
            Err(EnvError::ValueTooLong { .. })
        ));
    }

    #[test]
    fn patterns_accept_names_and_trailing_prefixes_only() {
        for pattern in ["PATH", "LC_*", "X*", "_*", "__CFBundleIdentifier"] {
            validate_pattern(pattern).expect("valid pattern");
        }
        for pattern in ["", "*", "**", "A*B", "*A", "A**", "1A", "A-B*", "A B"] {
            assert!(
                matches!(
                    validate_pattern(pattern),
                    Err(EnvError::InvalidPattern { .. })
                ),
                "{pattern:?} must be rejected"
            );
        }
        assert!(matches!(
            BaseEnv::from_allowlist(&["*"], vars(&[("PATH", "/bin")])),
            Err(EnvError::InvalidPattern { .. })
        ));
    }

    #[test]
    fn shipped_pattern_lists_are_valid() {
        for pattern in DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .chain(SERVICE_MANAGER_DENYLIST)
        {
            validate_pattern(pattern).expect("shipped pattern is valid");
        }
        for pattern in DEFAULT_ENVIRONMENT_ALLOWLIST {
            assert!(
                !is_denylisted(pattern.trim_end_matches(PREFIX_WILDCARD)),
                "{pattern} must not overlap the denylist"
            );
        }
    }
}
