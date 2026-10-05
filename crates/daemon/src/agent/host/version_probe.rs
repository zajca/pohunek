//! Supported-version policy of the data-driven version probe.
//!
//! A runtime descriptor that names the [`SEMVER_PARSER_ID`] or
//! [`SEMVER_LINE_PARSER_ID`] parser declares the probe arguments and the
//! half-open release range `[min, below)` the runtime is supported for, so a
//! package update can move the range without a daemon release.
//!
//! `semver-v1` reads a first output line that is exactly one release,
//! `MAJOR.MINOR.PATCH`. `semver-line-v1` reads a first output line of the shape
//! the descriptor's `line` template declares (see [`LineTemplate`]), which
//! covers the official runtimes: `codex-cli 0.160.0`, `2.1.289 (Claude Code)`
//! and `Hermes Agent v0.20.0 (2026.8.3)`. Both grammars take the release as a
//! bare numeric triple: a pre-release or build suffix cannot be ordered against
//! `[min, below)`, so a line carrying one is an unsupported runtime. Anything
//! unreadable is an unsupported runtime too.

// Rust guideline compliant 2026-10-04

use std::fmt;

use super::definition::{DefinitionError, MAX_ARG_BYTES};

/// Parser id of the data-driven version probe.
pub const SEMVER_PARSER_ID: &str = "semver-v1";

/// Parser id of the data-driven version probe whose first output line follows
/// a declared [`LineTemplate`].
pub const SEMVER_LINE_PARSER_ID: &str = "semver-line-v1";

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

/// Maximum length of a `line` template, in bytes.
///
/// A template is a short literal frame around one release; a longer one is a
/// descriptor that matches something other than a version banner.
const MAX_LINE_TEMPLATE_BYTES: usize = 128;

/// Maximum length of the first output line a [`LineTemplate`] reads, in bytes.
///
/// Real banners are under 64 bytes. The bound keeps the match work and the
/// annotation a runtime can smuggle into the inventory small.
const MAX_OUTPUT_LINE_BYTES: usize = 256;

/// Placeholder of a `line` template that stands for the release.
const VERSION_PLACEHOLDER: &str = "{version}";

/// Trailing placeholder of a `line` template that stands for the rest of the
/// line after one separating space.
const ANNOTATION_PLACEHOLDER: &str = "{annotation}";

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

/// Why a `line` template is not a valid output grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LineTemplateError {
    /// The template is empty or longer than the bound.
    #[error("must be 1..=128 bytes")]
    Length,
    /// The template holds a byte outside printable ASCII.
    #[error("must be printable ASCII")]
    NotPrintable,
    /// `{version}` is absent or repeated.
    #[error("must hold {{version}} exactly once")]
    Version,
    /// A brace that is not `{version}` or the trailing ` {annotation}`.
    #[error("only {{version}} and a trailing ` {{annotation}}` are placeholders")]
    Placeholder,
    /// The template starts or ends with a space; the output line is trimmed.
    #[error("must not start or end with a space")]
    EdgeSpace,
    /// The literal text before `{version}` ends in a digit or dot.
    #[error("the text before {{version}} must not end in a digit or `.`")]
    PrefixBoundary,
    /// The literal text after `{version}` starts with a digit, `.`, `-` or `+`.
    #[error("the text after {{version}} must not start with a digit, `.`, `-` or `+`")]
    SuffixBoundary,
    /// No literal text surrounds the release, so any line would match.
    #[error("must hold literal text next to {{version}}")]
    Unanchored,
}

impl LineTemplateError {
    /// A static description for [`DefinitionError::Field`].
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Length => "must be 1..=128 bytes",
            Self::NotPrintable => "must be printable ASCII",
            Self::Version => "must hold {version} exactly once",
            Self::Placeholder => "only {version} and a trailing ' {annotation}' are placeholders",
            Self::EdgeSpace => "must not start or end with a space",
            Self::PrefixBoundary => "the text before {version} must not end in a digit or '.'",
            Self::SuffixBoundary => {
                "the text after {version} must not start with a digit, '.', '-' or '+'"
            }
            Self::Unanchored => "must hold literal text next to {version}",
        }
    }
}

/// A closed one-line output grammar: literal prefix, one release, literal
/// suffix and an optional free-form annotation.
///
/// The template is `PREFIX{version}SUFFIX`, optionally followed by
/// ` {annotation}`. `{version}` is a bare `MAJOR.MINOR.PATCH` (see
/// [`ProbeVersion::parse`]); the annotation, when declared, is a space and at
/// least one more character on the same line. The match is anchored at both
/// ends of the trimmed line and linear: it compares literals and scans the
/// release token once, with no backtracking.
///
/// ```text
/// codex-cli {version}                  codex-cli 0.160.0
/// {version} (Claude Code)              2.1.289 (Claude Code)
/// Hermes Agent v{version} {annotation} Hermes Agent v0.20.0 (2026.8.3)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTemplate {
    prefix: String,
    suffix: String,
    annotation: bool,
}

impl LineTemplate {
    /// Parses and validates a `line` template.
    ///
    /// # Errors
    ///
    /// Returns the [`LineTemplateError`] naming the first violated rule.
    pub fn parse(template: &str) -> Result<Self, LineTemplateError> {
        if template.is_empty() || template.len() > MAX_LINE_TEMPLATE_BYTES {
            return Err(LineTemplateError::Length);
        }
        if !template.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            return Err(LineTemplateError::NotPrintable);
        }
        if template.starts_with(' ') || template.ends_with(' ') {
            return Err(LineTemplateError::EdgeSpace);
        }
        if template.matches(VERSION_PLACEHOLDER).count() != 1 {
            return Err(LineTemplateError::Version);
        }
        let (prefix, rest) = template
            .split_once(VERSION_PLACEHOLDER)
            .ok_or(LineTemplateError::Version)?;
        let (suffix, annotation) = match rest.strip_suffix(ANNOTATION_PLACEHOLDER) {
            Some(before) => (
                before
                    .strip_suffix(' ')
                    .ok_or(LineTemplateError::Placeholder)?,
                true,
            ),
            None => (rest, false),
        };
        if [prefix, suffix]
            .iter()
            .any(|literal| literal.contains(['{', '}']))
        {
            return Err(LineTemplateError::Placeholder);
        }
        if suffix.ends_with(' ') {
            return Err(LineTemplateError::EdgeSpace);
        }
        if prefix.ends_with(|c: char| c.is_ascii_digit() || c == '.') {
            return Err(LineTemplateError::PrefixBoundary);
        }
        if suffix.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')) {
            return Err(LineTemplateError::SuffixBoundary);
        }
        if prefix.is_empty() && suffix.is_empty() {
            return Err(LineTemplateError::Unanchored);
        }
        Ok(Self {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
            annotation,
        })
    }

    /// Reads the release from one trimmed output line, or `None` when the line
    /// does not have exactly the declared shape.
    #[must_use]
    pub fn read(&self, line: &str) -> Option<ProbeVersion> {
        let rest = line.strip_prefix(self.prefix.as_str())?;
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        let version = ProbeVersion::parse(token)?;
        let tail = tail.strip_prefix(self.suffix.as_str())?;
        let tail_matches = if self.annotation {
            tail.strip_prefix(' ')
                .is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control))
        } else {
            tail.is_empty()
        };
        tail_matches.then_some(version)
    }
}

/// How a policy reads the probe output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputGrammar {
    /// The first line is exactly one release.
    Bare,
    /// The first line follows a declared template.
    Line(LineTemplate),
}

/// The probe arguments and supported release range a descriptor declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionProbePolicy {
    args: Vec<String>,
    min: ProbeVersion,
    below: ProbeVersion,
    grammar: OutputGrammar,
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
        Ok(Self {
            args,
            min,
            below,
            grammar: OutputGrammar::Bare,
        })
    }

    /// Validates a policy whose output is read through a `line` template; the
    /// other fields follow [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming the offending field.
    pub fn with_line_template(
        args: Vec<String>,
        min: &str,
        below: &str,
        line: &str,
    ) -> Result<Self, DefinitionError> {
        let mut policy = Self::new(args, min, below)?;
        let template = LineTemplate::parse(line).map_err(|error| DefinitionError::Field {
            field: "runtime.version_probe.line",
            reason: error.reason(),
        })?;
        policy.grammar = OutputGrammar::Line(template);
        Ok(policy)
    }

    /// Whether the output is read through a `line` template.
    #[must_use]
    pub fn has_line_template(&self) -> bool {
        matches!(self.grammar, OutputGrammar::Line(_))
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

    /// Reads the release from probe output under this policy's grammar. Only
    /// the trimmed first line is read; later lines are ignored.
    #[must_use]
    pub fn read_output(&self, output: &str) -> Option<ProbeVersion> {
        match &self.grammar {
            OutputGrammar::Bare => Self::parse_output(output),
            OutputGrammar::Line(template) => {
                let line = output.lines().next()?.trim();
                if line.len() > MAX_OUTPUT_LINE_BYTES {
                    return None;
                }
                template.read(line)
            }
        }
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

    const CODEX_LINE: &str = "codex-cli {version}";
    const CLAUDE_LINE: &str = "{version} (Claude Code)";
    const HERMES_LINE: &str = "Hermes Agent v{version} {annotation}";

    fn line_policy(line: &str) -> VersionProbePolicy {
        VersionProbePolicy::with_line_template(vec!["--version".to_owned()], "0.1.0", "3.0.0", line)
            .expect("line policy")
    }

    fn line_release(line: &str, output: &str) -> Option<String> {
        line_policy(line)
            .read_output(output)
            .map(|version| version.to_string())
    }

    #[test]
    fn the_real_official_outputs_read_through_their_templates() {
        assert_eq!(
            line_release(CODEX_LINE, "codex-cli 0.160.0\n"),
            Some("0.160.0".to_owned())
        );
        assert_eq!(
            line_release(CLAUDE_LINE, "2.1.289 (Claude Code)\n"),
            Some("2.1.289".to_owned())
        );
        assert_eq!(
            line_release(HERMES_LINE, "Hermes Agent v0.20.0 (2026.8.3)\n"),
            Some("0.20.0".to_owned())
        );
        assert_eq!(
            line_release(
                HERMES_LINE,
                "Hermes Agent v0.20.0 (2026.8.3)\nProject: /x\nPython: 3.12\n"
            ),
            Some("0.20.0".to_owned()),
            "lines after the first are ignored"
        );
        assert_eq!(
            line_release(CODEX_LINE, "  codex-cli 0.160.0  \r\n"),
            Some("0.160.0".to_owned()),
            "the line is trimmed"
        );
    }

    #[test]
    fn malformed_outputs_are_not_a_release() {
        for (line, output) in [
            (CODEX_LINE, ""),
            (CODEX_LINE, "\ncodex-cli 0.160.0\n"),
            (CODEX_LINE, "0.160.0"),
            (CODEX_LINE, "codex 0.160.0"),
            (CODEX_LINE, "codex-cli  0.160.0"),
            (CODEX_LINE, "codex-cli 0.160"),
            (CODEX_LINE, "codex-cli 0.160.0.1"),
            (CODEX_LINE, "codex-cli 00.160.0"),
            (CODEX_LINE, "codex-cli v0.160.0"),
            (CODEX_LINE, "codex-cli 0.160.0-alpha.1"),
            (CODEX_LINE, "codex-cli 0.160.0+build"),
            (CODEX_LINE, "codex-cli 0.160.0 extra"),
            (CODEX_LINE, "codex-cli 0.160.99999999999999999999"),
            (CODEX_LINE, "prefix codex-cli 0.160.0"),
            (CLAUDE_LINE, "2.1.289"),
            (CLAUDE_LINE, "2.1.289 (Claude Code) extra"),
            (CLAUDE_LINE, "2.1.289-beta (Claude Code)"),
            (CLAUDE_LINE, "2.1.289  (Claude Code)"),
            (CLAUDE_LINE, "(Claude Code) 2.1.289"),
            (HERMES_LINE, "Hermes Agent v0.20.0"),
            (HERMES_LINE, "Hermes Agent v0.20.0 "),
            (HERMES_LINE, "Hermes Agent v0.20.0-rc.1 (2026.8.3)"),
            (HERMES_LINE, "Hermes Agent 0.20.0 (2026.8.3)"),
            (HERMES_LINE, "Hermes Agent v0.20.0\t(2026.8.3)"),
            (HERMES_LINE, "Hermes Agent v0.20.0 (2026\t.8.3)"),
        ] {
            assert_eq!(line_release(line, output), None, "{line:?} / {output:?}");
        }
    }

    #[test]
    fn a_huge_output_is_bounded_not_scanned_into_a_release() {
        let policy = line_policy(HERMES_LINE);
        let annotation = "x".repeat(MAX_OUTPUT_LINE_BYTES);
        assert_eq!(
            policy.read_output(&format!("Hermes Agent v0.20.0 {annotation}\n")),
            None
        );
        let digits = "9".repeat(1 << 20);
        assert_eq!(
            policy.read_output(&format!("Hermes Agent v0.20.{digits}")),
            None
        );
        assert_eq!(
            policy.read_output(&format!(
                "{}\nHermes Agent v0.20.0 (x)",
                " ".repeat(1 << 20)
            )),
            None
        );
        let at_limit = format!(
            "Hermes Agent v0.20.0 {}",
            "x".repeat(MAX_OUTPUT_LINE_BYTES - 21)
        );
        assert_eq!(at_limit.len(), MAX_OUTPUT_LINE_BYTES);
        assert!(policy.read_output(&at_limit).is_some());
    }

    #[test]
    fn valid_templates_compile_and_invalid_ones_name_their_rule() {
        for valid in [
            CODEX_LINE,
            CLAUDE_LINE,
            HERMES_LINE,
            "v{version}",
            "{version} cli",
            "{version}!",
        ] {
            assert!(LineTemplate::parse(valid).is_ok(), "{valid:?}");
        }
        let long = format!("{}{{version}}", "a".repeat(MAX_LINE_TEMPLATE_BYTES));
        for (invalid, error) in [
            ("", LineTemplateError::Length),
            (long.as_str(), LineTemplateError::Length),
            ("tool \u{e9}{version}", LineTemplateError::NotPrintable),
            ("tool\t{version}", LineTemplateError::NotPrintable),
            ("tool\n{version}", LineTemplateError::NotPrintable),
            ("tool", LineTemplateError::Version),
            ("{version} {version}", LineTemplateError::Version),
            (" tool {version}", LineTemplateError::EdgeSpace),
            ("tool {version} ", LineTemplateError::EdgeSpace),
            ("tool {version}  {annotation}", LineTemplateError::EdgeSpace),
            ("tool {version}{annotation}", LineTemplateError::Placeholder),
            (
                "{annotation} tool {version}",
                LineTemplateError::Placeholder,
            ),
            (
                "tool {version} {annotation} {annotation}",
                LineTemplateError::Placeholder,
            ),
            ("tool {version} {other}", LineTemplateError::Placeholder),
            ("tool 1{version}", LineTemplateError::PrefixBoundary),
            ("tool.{version}", LineTemplateError::PrefixBoundary),
            ("{version}1", LineTemplateError::SuffixBoundary),
            ("{version}.x", LineTemplateError::SuffixBoundary),
            ("{version}-rc", LineTemplateError::SuffixBoundary),
            ("{version}+x", LineTemplateError::SuffixBoundary),
            ("{version}", LineTemplateError::Unanchored),
            ("{version} {annotation}", LineTemplateError::Unanchored),
        ] {
            assert_eq!(LineTemplate::parse(invalid), Err(error), "{invalid:?}");
        }
    }

    #[test]
    fn an_invalid_template_is_a_field_error_at_policy_construction() {
        for line in ["", "{version}", "tool {version} {other}", "tool"] {
            match VersionProbePolicy::with_line_template(
                vec!["--version".to_owned()],
                "1.0.0",
                "2.0.0",
                line,
            ) {
                Err(DefinitionError::Field { field, .. }) => {
                    assert_eq!(field, "runtime.version_probe.line", "{line:?}");
                }
                other => panic!("expected a field error for {line:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_bare_grammar_is_unchanged_by_the_template_grammar() {
        let bare = policy();
        assert!(!bare.has_line_template());
        for (output, expected) in [
            ("1.0.2\n", Some("1.0.2")),
            ("  1.0.2  \r\nignored\n", Some("1.0.2")),
            ("codex-cli 0.160.0", None),
            ("1.0.2-beta", None),
            ("", None),
        ] {
            assert_eq!(
                bare.read_output(output).map(|version| version.to_string()),
                expected.map(str::to_owned),
                "{output:?}"
            );
            assert_eq!(
                VersionProbePolicy::parse_output(output).map(|version| version.to_string()),
                expected.map(str::to_owned),
                "{output:?}"
            );
        }
    }

    #[test]
    fn templates_are_literal_text_not_patterns() {
        assert_eq!(line_release(".*{version}", "banana 1.0.0"), None);
        assert_eq!(
            line_release(".*{version}", ".*1.0.0"),
            Some("1.0.0".to_owned())
        );
        assert_eq!(line_release("a|b {version}", "a 1.0.0"), None);
    }

    #[test]
    fn an_annotation_needs_text_after_its_separator() {
        let template = LineTemplate::parse(HERMES_LINE).expect("template");
        assert!(template.read("Hermes Agent v0.20.0 (x)").is_some());
        assert_eq!(template.read("Hermes Agent v0.20.0 "), None);
        assert_eq!(template.read("Hermes Agent v0.20.0"), None);
    }
}
