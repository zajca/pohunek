//! Supported-version policy of the data-driven version probe.
//!
//! A runtime descriptor that names the [`SEMVER_PARSER_ID`] parser declares the
//! probe arguments and the half-open release range `[min, below)` the runtime
//! is supported for, so a package update can move the range without a daemon
//! release. The probe prints one release as `MAJOR.MINOR.PATCH` on the first
//! line of its standard output; anything else is an unsupported runtime.

// Rust guideline compliant 2026-10-04

use std::fmt;

use super::definition::{DefinitionError, MAX_ARG_BYTES};

/// Parser id of the data-driven version probe.
pub const SEMVER_PARSER_ID: &str = "semver-v1";

/// Maximum number of probe arguments.
///
/// A probe asks for a version (`--version`, or a subcommand and a flag); a
/// longer argv is a descriptor that does something else.
pub const MAX_PROBE_ARGS: usize = 4;

/// Maximum accepted length of a version string, in bytes.
///
/// Probe output is untrusted; the bound keeps parsing work and the reported
/// inventory string small. Real releases are under 16 bytes.
const MAX_VERSION_BYTES: usize = 64;

/// A `MAJOR.MINOR.PATCH` release without pre-release or build metadata.
///
/// Components compare numerically, so `1.10.0` is newer than `1.9.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProbeVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl ProbeVersion {
    /// Parses exactly `MAJOR.MINOR.PATCH`: ASCII digits only, no leading
    /// zeros, no sign, no pre-release or build suffix.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > MAX_VERSION_BYTES {
            return None;
        }
        let mut parts = value.split('.');
        let mut component = || {
            let part = parts.next()?;
            let canonical = !part.is_empty()
                && (part == "0" || !part.starts_with('0'))
                && part.bytes().all(|byte| byte.is_ascii_digit());
            if canonical {
                part.parse::<u64>().ok()
            } else {
                None
            }
        };
        let version = Self {
            major: component()?,
            minor: component()?,
            patch: component()?,
        };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for ProbeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The probe arguments and supported release range a descriptor declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionProbePolicy {
    args: Vec<String>,
    min: ProbeVersion,
    below: ProbeVersion,
}

impl VersionProbePolicy {
    /// Validates a policy: 1..=[`MAX_PROBE_ARGS`] argument tokens, a `min`
    /// release and a strictly greater `below` release.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming the offending field.
    pub fn new(args: Vec<String>, min: &str, below: &str) -> Result<Self, DefinitionError> {
        const ARGS: &str = "runtime.version_probe.args";
        if args.is_empty() || args.len() > MAX_PROBE_ARGS {
            return Err(DefinitionError::Field {
                field: ARGS,
                reason: "must hold 1..=4 arguments",
            });
        }
        let valid_token = |arg: &String| {
            !arg.is_empty() && arg.len() <= MAX_ARG_BYTES && !arg.chars().any(char::is_control)
        };
        if !args.iter().all(valid_token) {
            return Err(DefinitionError::Field {
                field: ARGS,
                reason: "must be 1..=1024 bytes without control characters",
            });
        }
        let min = ProbeVersion::parse(min).ok_or(DefinitionError::Field {
            field: "runtime.version_probe.min",
            reason: "must be MAJOR.MINOR.PATCH",
        })?;
        let below = ProbeVersion::parse(below).ok_or(DefinitionError::Field {
            field: "runtime.version_probe.below",
            reason: "must be MAJOR.MINOR.PATCH",
        })?;
        if min >= below {
            return Err(DefinitionError::Field {
                field: "runtime.version_probe.below",
                reason: "must be greater than min",
            });
        }
        Ok(Self { args, min, below })
    }

    /// The arguments the probe passes to the runtime program.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Lowest supported release, inclusive.
    #[must_use]
    pub fn min(&self) -> ProbeVersion {
        self.min
    }

    /// First release that is no longer supported.
    #[must_use]
    pub fn below(&self) -> ProbeVersion {
        self.below
    }

    /// Whether `version` lies in `[min, below)`.
    #[must_use]
    pub fn supports(&self, version: ProbeVersion) -> bool {
        self.min <= version && version < self.below
    }

    /// Reads the release from probe output: the trimmed first line must be
    /// exactly one release.
    #[must_use]
    pub fn parse_output(output: &str) -> Option<ProbeVersion> {
        ProbeVersion::parse(output.lines().next()?.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> VersionProbePolicy {
        VersionProbePolicy::new(vec!["--version".to_owned()], "1.0.0", "1.1.0").expect("policy")
    }

    #[test]
    fn versions_parse_strictly() {
        for valid in ["0.0.0", "1.0.2", "10.20.30"] {
            assert_eq!(
                ProbeVersion::parse(valid).map(|version| version.to_string()),
                Some(valid.to_owned())
            );
        }
        for invalid in [
            "",
            "1",
            "1.0",
            "1.0.0.0",
            "01.0.0",
            "1.0.0-rc.1",
            "1.0.0+b",
            "v1.0.0",
            "1.0.x",
            "+1.0.0",
            "1. 0.0",
            " 1.0.0",
            "1.0.0\n",
        ] {
            assert_eq!(ProbeVersion::parse(invalid), None, "{invalid:?}");
        }
        assert_eq!(
            ProbeVersion::parse(&format!("1.0.{}", "9".repeat(80))),
            None
        );
        assert_eq!(ProbeVersion::parse("1.0.99999999999999999999"), None);
    }

    #[test]
    fn the_range_is_half_open_and_numeric() {
        let policy = policy();
        let supports = |text: &str| policy.supports(ProbeVersion::parse(text).expect("version"));
        assert!(supports("1.0.0"));
        assert!(supports("1.0.2"));
        assert!(supports("1.0.99"));
        assert!(!supports("0.99.99"));
        assert!(!supports("1.1.0"));
        assert!(!supports("2.0.0"));
        let wide = VersionProbePolicy::new(vec!["-V".to_owned()], "1.9.0", "1.10.0").expect("wide");
        assert!(wide.supports(ProbeVersion::parse("1.9.5").expect("version")));
        assert!(!wide.supports(ProbeVersion::parse("1.10.0").expect("version")));
    }

    #[test]
    fn output_is_the_trimmed_first_line() {
        let release = |text: &str| VersionProbePolicy::parse_output(text).map(|v| v.to_string());
        assert_eq!(release("1.0.2\n"), Some("1.0.2".to_owned()));
        assert_eq!(release("  1.0.2  \r\nignored\n"), Some("1.0.2".to_owned()));
        assert_eq!(release(""), None);
        assert_eq!(
            release("\n1.0.2\n"),
            None,
            "an empty first line is no release"
        );
        assert_eq!(release("pi 1.0.2\n"), None);
        assert_eq!(release("1.0.2-beta\n"), None);
    }

    #[test]
    fn invalid_policies_name_the_field() {
        let field = |args: Vec<&str>, min: &str, below: &str| match VersionProbePolicy::new(
            args.into_iter().map(str::to_owned).collect(),
            min,
            below,
        ) {
            Err(DefinitionError::Field { field, .. }) => field,
            other => panic!("expected a field error, got {other:?}"),
        };
        assert_eq!(
            field(vec![], "1.0.0", "1.1.0"),
            "runtime.version_probe.args"
        );
        assert_eq!(
            field(vec!["a", "b", "c", "d", "e"], "1.0.0", "1.1.0"),
            "runtime.version_probe.args"
        );
        assert_eq!(
            field(vec![""], "1.0.0", "1.1.0"),
            "runtime.version_probe.args"
        );
        assert_eq!(
            field(vec!["--ver\nsion"], "1.0.0", "1.1.0"),
            "runtime.version_probe.args"
        );
        assert_eq!(
            field(vec!["-V"], "1.0", "1.1.0"),
            "runtime.version_probe.min"
        );
        assert_eq!(
            field(vec!["-V"], "1.0.0", "x"),
            "runtime.version_probe.below"
        );
        assert_eq!(
            field(vec!["-V"], "1.1.0", "1.1.0"),
            "runtime.version_probe.below"
        );
        assert_eq!(
            field(vec!["-V"], "2.0.0", "1.1.0"),
            "runtime.version_probe.below"
        );
    }
}
