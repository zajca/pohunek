use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use crate::procwatch::ProcessFact;
use protocol::AgentActivity;
use regex::Regex;

mod error;
mod matcher;
mod parser;

pub use error::{ManifestError, MatcherKind};

use matcher::MatchEvaluation;
use parser::{
    compile_process_matchers, ComplexityBudget, RawManifest, Rule, MAX_RULES, MAX_SOURCE_BYTES,
};

// `MAX_MATCHER_BYTES` is only referenced by the test module (which sees it via
// `use super::*`); `MAX_SOURCE_BYTES` is already in scope for `parse_str`.
/// Largest manifest source the parser accepts, for readers that must bound a
/// file before they hold it in memory.
pub(crate) const MAX_MANIFEST_SOURCE_BYTES: usize = MAX_SOURCE_BYTES;

#[cfg(test)]
use parser::{MAX_MATCHERS, MAX_MATCHER_BYTES};

#[derive(Debug, Clone)]
pub struct Manifest {
    rules: Vec<Rule>,
    process: Option<ProcessMatchers>,
}

impl Manifest {
    pub fn parse_str(source: &str) -> Result<Self, ManifestError> {
        if source.len() > MAX_SOURCE_BYTES {
            return Err(ManifestError::SourceTooLarge {
                size: source.len(),
                max: MAX_SOURCE_BYTES,
            });
        }

        let raw = toml::from_str::<RawManifest>(source)?;
        if raw.rules.len() > MAX_RULES {
            return Err(ManifestError::TooManyRules {
                count: raw.rules.len(),
                max: MAX_RULES,
            });
        }

        let mut budget = ComplexityBudget::default();
        let mut rules = Vec::with_capacity(raw.rules.len());
        for raw_rule in raw.rules {
            rules.push(Rule::try_from_raw(raw_rule, &mut budget)?);
        }
        let process = raw
            .process
            .map(|raw_process| compile_process_matchers(raw_process, &mut budget))
            .transpose()?;

        Ok(Self { rules, process })
    }

    #[must_use]
    pub fn match_context(&self, context: &MatchContext) -> Option<ManifestMatch> {
        let mut evaluation = MatchEvaluation::new(context);
        let mut best_match = None;

        for rule in &self.rules {
            if !evaluation.has_region_text(&rule.region) {
                continue;
            }

            if !rule.gate.matches(&rule.region, &mut evaluation) {
                continue;
            }

            if best_match
                .as_ref()
                .is_none_or(|current: &&Rule| rule.priority > current.priority)
            {
                best_match = Some(rule);
            }
        }

        best_match.map(|rule| ManifestMatch {
            rule_id: rule.id.clone(),
            activity: rule.activity,
            priority: rule.priority,
            region: rule.region.clone(),
            visible_blocker: rule.visible_blocker,
        })
    }

    #[must_use]
    pub fn required_regions(&self) -> Vec<ManifestRegion> {
        let mut regions = Vec::new();

        for rule in &self.rules {
            if !regions.contains(&rule.region) {
                regions.push(rule.region.clone());
            }
        }

        regions
    }

    /// Returns process matchers when this manifest declares `[process]`.
    #[must_use]
    pub fn process_matchers(&self) -> Option<&ProcessMatchers> {
        self.process.as_ref()
    }
}

/// Process-level matchers compiled from a manifest `[process]` section.
#[derive(Debug, Clone)]
pub struct ProcessMatchers {
    comm: Vec<Regex>,
    cmdline: Vec<Regex>,
}

impl ProcessMatchers {
    pub(super) fn new(comm: Vec<Regex>, cmdline: Vec<Regex>) -> Self {
        Self { comm, cmdline }
    }

    /// Returns whether process facts match this manifest.
    ///
    /// Matching is an OR: any `comm` regex matching the task command name, or any
    /// `cmdline` regex matching argv joined with spaces, is enough. An empty
    /// `[process]` section never matches.
    #[must_use]
    pub fn matches(&self, fact: &ProcessFact) -> bool {
        self.comm.iter().any(|regex| regex.is_match(&fact.comm))
            || (!self.cmdline.is_empty()
                && self
                    .cmdline
                    .iter()
                    .any(|regex| regex.is_match(&fact.cmdline.join(" "))))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestMatch {
    pub rule_id: String,
    pub activity: AgentActivity,
    pub priority: i32,
    pub region: ManifestRegion,
    pub visible_blocker: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ManifestRegion {
    OscTitle,
    OscProgress,
    WholeRecent,
    BottomLines(usize),
    BottomNonEmptyLines(usize),
    TopNonEmptyLines(usize),
    LastNonEmptyAbovePromptBox,
    AfterLastPromptMarker,
    PromptBoxBody,
    AfterLastHorizontalRule,
}

impl ManifestRegion {
    /// Returns the public diagnostic kind for this manifest region.
    #[must_use]
    pub const fn kind(&self) -> protocol::DetectionRegionKind {
        match self {
            Self::OscTitle => protocol::DetectionRegionKind::OscTitle,
            Self::OscProgress => protocol::DetectionRegionKind::OscProgress,
            Self::WholeRecent => protocol::DetectionRegionKind::WholeRecent,
            Self::BottomLines(_) => protocol::DetectionRegionKind::BottomLines,
            Self::BottomNonEmptyLines(_) => protocol::DetectionRegionKind::BottomNonEmptyLines,
            Self::TopNonEmptyLines(_) => protocol::DetectionRegionKind::TopNonEmptyLines,
            Self::LastNonEmptyAbovePromptBox => {
                protocol::DetectionRegionKind::LastNonEmptyAbovePromptBox
            }
            Self::AfterLastPromptMarker => protocol::DetectionRegionKind::AfterLastPromptMarker,
            Self::PromptBoxBody => protocol::DetectionRegionKind::PromptBoxBody,
            Self::AfterLastHorizontalRule => protocol::DetectionRegionKind::AfterLastHorizontalRule,
        }
    }
}

impl fmt::Display for ManifestRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OscTitle => f.write_str("osc_title"),
            Self::OscProgress => f.write_str("osc_progress"),
            Self::WholeRecent => f.write_str("whole_recent"),
            Self::BottomLines(count) => write!(f, "bottom_lines({count})"),
            Self::BottomNonEmptyLines(count) => {
                write!(f, "bottom_non_empty_lines({count})")
            }
            Self::TopNonEmptyLines(count) => {
                write!(f, "top_non_empty_lines({count})")
            }
            Self::LastNonEmptyAbovePromptBox => f.write_str("last_non_empty_above_prompt_box"),
            Self::AfterLastPromptMarker => f.write_str("after_last_prompt_marker"),
            Self::PromptBoxBody => f.write_str("prompt_box_body"),
            Self::AfterLastHorizontalRule => f.write_str("after_last_horizontal_rule"),
        }
    }
}

impl FromStr for ManifestRegion {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "osc_title" => Ok(Self::OscTitle),
            "osc_progress" => Ok(Self::OscProgress),
            "whole_recent" => Ok(Self::WholeRecent),
            "after_last_prompt_marker" => Ok(Self::AfterLastPromptMarker),
            "prompt_box_body" => Ok(Self::PromptBoxBody),
            "after_last_horizontal_rule" => Ok(Self::AfterLastHorizontalRule),
            "last_non_empty_above_prompt_box" => Ok(Self::LastNonEmptyAbovePromptBox),
            _ => parse_parameterized_region(value).ok_or(()),
        }
    }
}

fn parse_parameterized_region(value: &str) -> Option<ManifestRegion> {
    parse_region_count(value, "bottom_lines")
        .map(ManifestRegion::BottomLines)
        .or_else(|| {
            parse_region_count(value, "bottom_non_empty_lines")
                .map(ManifestRegion::BottomNonEmptyLines)
        })
        .or_else(|| {
            parse_region_count(value, "top_non_empty_lines").map(ManifestRegion::TopNonEmptyLines)
        })
}

fn parse_region_count(value: &str, name: &str) -> Option<usize> {
    value
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?
        .parse()
        .ok()
}

#[derive(Debug, Default)]
pub struct MatchContext {
    regions: HashMap<ManifestRegion, String>,
}

impl MatchContext {
    #[must_use]
    pub fn with_region_text(mut self, region: ManifestRegion, text: impl Into<String>) -> Self {
        self.regions.insert(region, text.into());
        self
    }

    pub(super) fn region_text(&self, region: &ManifestRegion) -> Option<&str> {
        self.regions.get(region).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highest_priority_matching_rule_wins() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "low"
            state = "idle"
            priority = 10
            region = "whole_recent"
            contains = "ready"

            [[rules]]
            id = "high"
            state = "working"
            priority = 20
            region = "whole_recent"
            contains = "ready"
            "#,
        )
        .expect("manifest should parse");

        let context =
            MatchContext::default().with_region_text(ManifestRegion::WholeRecent, "agent is ready");

        assert_eq!(
            manifest.match_context(&context),
            Some(ManifestMatch {
                rule_id: "high".to_string(),
                activity: AgentActivity::Working,
                priority: 20,
                region: ManifestRegion::WholeRecent,
                visible_blocker: false,
            })
        );
    }

    #[test]
    fn process_section_matches_comm_or_joined_cmdline() {
        let manifest = Manifest::parse_str(
            r#"
            [process]
            comm = ["^codex$"]
            cmdline = ["(^|/)codex($| )"]
            "#,
        )
        .expect("manifest should parse");
        let matchers = manifest
            .process_matchers()
            .expect("process section should compile");

        assert!(matchers.matches(&ProcessFact {
            pid: 100,
            pgid: 100,
            ppid: 1,
            start_identity: crate::procwatch::StartIdentity::new(100),
            comm: "codex".to_owned(),
            cmdline: vec!["/usr/bin/other".to_owned()],
        }));
        assert!(matchers.matches(&ProcessFact {
            pid: 101,
            pgid: 101,
            ppid: 1,
            start_identity: crate::procwatch::StartIdentity::new(101),
            comm: "sleep".to_owned(),
            cmdline: vec!["/work/tools/codex".to_owned(), "30".to_owned()],
        }));
        assert!(!matchers.matches(&ProcessFact {
            pid: 102,
            pgid: 102,
            ppid: 1,
            start_identity: crate::procwatch::StartIdentity::new(102),
            comm: "sleep".to_owned(),
            cmdline: vec!["/work/tools/not-codex".to_owned(), "30".to_owned()],
        }));
    }

    #[test]
    fn process_matchers_count_against_manifest_complexity_budget() {
        let patterns = (0..=MAX_MATCHERS)
            .map(|index| format!("\"pattern-{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!(
            r"
            [process]
            cmdline = [{patterns}]
            "
        );

        let err = Manifest::parse_str(&source).expect_err("manifest should exceed matcher budget");

        assert!(matches!(err, ManifestError::TooManyMatchers { .. }));
    }

    #[test]
    fn equal_priority_matching_rules_keep_first_match() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "first"
            state = "idle"
            priority = 10
            region = "whole_recent"
            contains = "ready"

            [[rules]]
            id = "second"
            state = "working"
            priority = 10
            region = "whole_recent"
            contains = "ready"
            "#,
        )
        .expect("manifest should parse");

        let context =
            MatchContext::default().with_region_text(ManifestRegion::WholeRecent, "agent is ready");

        assert_eq!(
            manifest
                .match_context(&context)
                .map(|matched| matched.rule_id),
            Some("first".to_string())
        );
    }

    #[test]
    fn contains_is_case_insensitive() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "APPROVAL REQUIRED"
            "#,
        )
        .expect("manifest should parse");

        let context = MatchContext::default().with_region_text(
            ManifestRegion::WholeRecent,
            "approval required before running",
        );

        assert_eq!(
            manifest
                .match_context(&context)
                .map(|matched| matched.rule_id),
            Some("blocked".to_string())
        );
    }

    #[test]
    fn regex_matches_whole_region_and_line_regex_matches_per_line() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "whole"
            state = "working"
            priority = 5
            region = "whole_recent"
            regex = "building\\s+crate"

            [[rules]]
            id = "line"
            state = "blocked"
            priority = 10
            region = "whole_recent"
            line_regex = "^error:"
            "#,
        )
        .expect("manifest should parse");

        let context = MatchContext::default()
            .with_region_text(ManifestRegion::WholeRecent, "building crate\nerror: denied");

        assert_eq!(
            manifest
                .match_context(&context)
                .map(|matched| matched.rule_id),
            Some("line".to_string())
        );
    }

    #[test]
    fn nested_all_any_not_gates_work() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "nested"
            state = "working"
            priority = 1
            region = "whole_recent"

            [rules.gates]
            all = [
                { contains = "compiling" },
                { any = [
                    { contains = "crate" },
                    { contains = "workspace" },
                ] },
                { not = { contains = "error" } },
            ]
            "#,
        )
        .expect("manifest should parse");

        let matching = MatchContext::default()
            .with_region_text(ManifestRegion::WholeRecent, "Compiling workspace");
        let blocked = MatchContext::default()
            .with_region_text(ManifestRegion::WholeRecent, "Compiling workspace\nerror");

        assert!(manifest.match_context(&matching).is_some());
        assert_eq!(manifest.match_context(&blocked), None);
    }

    #[test]
    fn empty_gates_are_rejected() {
        for (rule_id, gate) in [
            ("empty-all", "all = []"),
            ("empty-any", "any = []"),
            ("not-empty-any", "not = { any = [] }"),
        ] {
            let source = format!(
                r#"
                [[rules]]
                id = "{rule_id}"
                state = "working"
                priority = 1
                region = "whole_recent"
                {gate}
                "#
            );

            let error = Manifest::parse_str(&source).expect_err(rule_id);

            assert!(
                matches!(&error, ManifestError::MissingGate { rule_id: id } if id == rule_id),
                "{rule_id}: {error:?}"
            );
        }
    }

    #[test]
    fn empty_matchers_are_rejected() {
        for (rule_id, matcher, expected_kind) in [
            ("empty-contains", r#"contains = """#, MatcherKind::Contains),
            ("empty-regex", r#"regex = """#, MatcherKind::Regex),
            (
                "empty-line-regex",
                r#"line_regex = """#,
                MatcherKind::LineRegex,
            ),
        ] {
            let source = format!(
                r#"
                [[rules]]
                id = "{rule_id}"
                state = "working"
                priority = 1
                region = "whole_recent"
                {matcher}
                "#
            );

            let error = Manifest::parse_str(&source).expect_err(rule_id);

            assert!(
                matches!(
                    &error,
                    ManifestError::EmptyMatcher { rule_id: id, kind }
                        if id == rule_id && *kind == expected_kind
                ),
                "{rule_id}: {error:?}"
            );
        }
    }

    #[test]
    fn unknown_manifest_keys_are_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "typo"
            state = "working"
            priority = 1
            region = "whole_recent"
            containz = "ready"
            "#,
        )
        .expect_err("unknown rule key should be rejected");

        assert!(matches!(error, ManifestError::Toml(_)));
    }

    #[test]
    fn parses_plan_rule_shape_with_root_gates_matcher_arrays_and_visible_blocker() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "live_blocked_form"
            state = "blocked"
            priority = 980
            region = "after_last_horizontal_rule"
            visible_blocker = true
            contains = ["enter to select", "esc to cancel"]
            any = [
                { contains = ["arrow keys to navigate"] },
                { contains = ["↑/↓ to navigate"] },
            ]
            "#,
        )
        .expect("manifest should parse");

        let context = MatchContext::default().with_region_text(
            ManifestRegion::AfterLastHorizontalRule,
            "enter to select\nesc to cancel\n↑/↓ to navigate",
        );
        let missing_required_text = MatchContext::default().with_region_text(
            ManifestRegion::AfterLastHorizontalRule,
            "enter to select\n↑/↓ to navigate",
        );

        let matched = manifest.match_context(&context).expect("rule should match");
        assert_eq!(matched.rule_id, "live_blocked_form");
        assert!(matched.visible_blocker);
        assert_eq!(manifest.match_context(&missing_required_text), None);
    }

    #[test]
    fn parses_parameterized_bottom_regions() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "bottom-lines"
            state = "idle"
            priority = 1
            region = "bottom_lines(3)"
            contains = "tail"

            [[rules]]
            id = "bottom-non-empty"
            state = "blocked"
            priority = 2
            region = "bottom_non_empty_lines(2)"
            contains = "prompt"
            "#,
        )
        .expect("manifest should parse");

        let context = MatchContext::default()
            .with_region_text(ManifestRegion::BottomLines(3), "tail")
            .with_region_text(ManifestRegion::BottomNonEmptyLines(2), "prompt");

        assert_eq!(
            manifest
                .match_context(&context)
                .map(|matched| matched.region),
            Some(ManifestRegion::BottomNonEmptyLines(2))
        );
    }

    #[test]
    fn new_regions_share_the_global_matcher_complexity_budget() {
        let top_matchers = (0..512)
            .map(|_| r#"{ contains = "top" }"#)
            .collect::<Vec<_>>()
            .join(",");
        let prompt_matchers = (0..513)
            .map(|_| r#"{ contains = "prompt" }"#)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            r#"
            [[rules]]
            id = "top"
            state = "blocked"
            priority = 2
            region = "top_non_empty_lines(8)"
            [rules.gates]
            all = [{top_matchers}]

            [[rules]]
            id = "prompt-adjacent"
            state = "idle"
            priority = 1
            region = "last_non_empty_above_prompt_box"
            [rules.gates]
            all = [{prompt_matchers}]
            "#,
        );

        assert!(matches!(
            Manifest::parse_str(&source),
            Err(ManifestError::TooManyMatchers {
                count: 1025,
                max: 1024
            })
        ));
    }

    #[test]
    fn required_regions_returns_unique_rule_regions_in_rule_order() {
        let manifest = Manifest::parse_str(
            r#"
            [[rules]]
            id = "title"
            state = "idle"
            priority = 1
            region = "osc_title"
            contains = "ready"

            [[rules]]
            id = "bottom"
            state = "blocked"
            priority = 2
            region = "bottom_lines(11)"
            contains = "approval"

            [[rules]]
            id = "duplicate-title"
            state = "working"
            priority = 3
            region = "osc_title"
            contains = "working"
            "#,
        )
        .expect("manifest should parse");

        assert_eq!(
            manifest.required_regions(),
            vec![ManifestRegion::OscTitle, ManifestRegion::BottomLines(11)]
        );
    }

    #[test]
    fn complexity_caps_reject_over_budget_manifests() {
        let too_many_rules = format!(
            "{}\n",
            (0..129)
                .map(|index| format!(
                    r#"[[rules]]
id = "rule-{index}"
state = "idle"
priority = {index}
region = "whole_recent"
contains = "x"
"#
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );

        assert!(matches!(
            Manifest::parse_str(&too_many_rules),
            Err(ManifestError::TooManyRules {
                count: 129,
                max: 128
            })
        ));

        let too_many_matchers = format!(
            r#"
            [[rules]]
            id = "many-matchers"
            state = "working"
            priority = 1
            region = "whole_recent"

            [rules.gates]
            all = [{}]
            "#,
            (0..1025)
                .map(|_| r#"{ contains = "x" }"#)
                .collect::<Vec<_>>()
                .join(",")
        );

        assert!(matches!(
            Manifest::parse_str(&too_many_matchers),
            Err(ManifestError::TooManyMatchers {
                count: 1025,
                max: 1024
            })
        ));

        let too_many_gates = format!(
            r#"
            [[rules]]
            id = "many-gates"
            state = "working"
            priority = 1
            region = "whole_recent"

            [rules.gates]
            all = [{}]
            "#,
            (0..513)
                .map(|_| r#"{ not = { contains = "x" } }"#)
                .collect::<Vec<_>>()
                .join(",")
        );

        assert!(matches!(
            Manifest::parse_str(&too_many_gates),
            Err(ManifestError::TooManyGates { max: 512, .. })
        ));

        let too_deep = r#"
            [[rules]]
            id = "too-deep"
            state = "working"
            priority = 1
            region = "whole_recent"

            [rules.gates]
            not = { not = { not = { not = { not = { not = { not = { not = { not = { contains = "x" } } } } } } } } }
            "#;

        assert!(matches!(
            Manifest::parse_str(too_deep),
            Err(ManifestError::MaxDepthExceeded { max: 8, .. })
        ));
    }

    #[test]
    fn source_and_matcher_string_caps_reject_over_budget_manifests() {
        let overlong_source = format!("{}\n", "#".repeat(MAX_SOURCE_BYTES + 1));

        assert!(matches!(
            Manifest::parse_str(&overlong_source),
            Err(ManifestError::SourceTooLarge {
                size,
                max: MAX_SOURCE_BYTES,
            }) if size == overlong_source.len()
        ));

        let overlong_contains = format!(
            r#"
            [[rules]]
            id = "long-contains"
            state = "working"
            priority = 1
            region = "whole_recent"
            contains = "{}"
            "#,
            "a".repeat(MAX_MATCHER_BYTES + 1)
        );

        assert!(matches!(
            Manifest::parse_str(&overlong_contains),
            Err(ManifestError::MatcherTooLong {
                rule_id,
                kind: MatcherKind::Contains,
                len,
                max: MAX_MATCHER_BYTES,
            }) if rule_id == "long-contains" && len == MAX_MATCHER_BYTES + 1
        ));

        let overlong_pattern = format!(
            r#"
            [[rules]]
            id = "long-regex"
            state = "working"
            priority = 1
            region = "whole_recent"
            regex = "{}"
            "#,
            "a".repeat(MAX_MATCHER_BYTES + 1)
        );

        assert!(matches!(
            Manifest::parse_str(&overlong_pattern),
            Err(ManifestError::MatcherTooLong {
                rule_id,
                kind: MatcherKind::Regex,
                len,
                max: MAX_MATCHER_BYTES,
            }) if rule_id == "long-regex" && len == MAX_MATCHER_BYTES + 1
        ));
    }

    #[test]
    fn invalid_state_returns_typed_manifest_error() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "bad-state"
            state = "waiting"
            priority = 1
            region = "whole_recent"
            contains = "ready"
            "#,
        )
        .expect_err("invalid state should be rejected");

        assert!(matches!(
            error,
            ManifestError::InvalidState { rule_id, state }
                if rule_id == "bad-state" && state == "waiting"
        ));
    }

    #[test]
    fn invalid_region_returns_typed_manifest_error() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "bad-region"
            state = "working"
            priority = 1
            region = "sidebar"
            contains = "ready"
            "#,
        )
        .expect_err("invalid region should be rejected");

        assert!(matches!(
            error,
            ManifestError::InvalidRegion { rule_id, region }
                if rule_id == "bad-region" && region == "sidebar"
        ));
    }

    #[test]
    fn invalid_regex_returns_typed_manifest_error() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "invalid-regex"
            state = "working"
            priority = 1
            region = "whole_recent"
            regex = "["
            "#,
        )
        .expect_err("invalid regex should be rejected");

        assert!(matches!(
            error,
            ManifestError::InvalidRegex { rule_id, .. } if rule_id == "invalid-regex"
        ));
    }
}
