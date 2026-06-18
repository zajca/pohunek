use std::collections::HashMap;
use std::str::FromStr;

use protocol::AgentActivity;
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

const MAX_RULES: usize = 128;
const MAX_GATES: usize = 512;
const MAX_MATCHERS: usize = 1024;
const MAX_DEPTH: usize = 8;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_MATCHER_BYTES: usize = 4096;

#[derive(Debug)]
pub struct Manifest {
    rules: Vec<Rule>,
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

        Ok(Self { rules })
    }

    pub fn match_context(&self, context: &MatchContext) -> Option<ManifestMatch> {
        let mut evaluation = MatchEvaluation::new(context);
        let mut best_match = None;

        for rule in &self.rules {
            if !evaluation.has_region_text(&rule.region) {
                continue;
            };

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
        })
    }

    pub fn required_regions(&self) -> Vec<ManifestRegion> {
        let mut regions = Vec::new();

        for rule in &self.rules {
            if !regions.contains(&rule.region) {
                regions.push(rule.region.clone());
            }
        }

        regions
    }
}

#[derive(Debug)]
struct MatchEvaluation<'a> {
    context: &'a MatchContext,
    lowercase_regions: HashMap<ManifestRegion, String>,
}

impl<'a> MatchEvaluation<'a> {
    fn new(context: &'a MatchContext) -> Self {
        Self {
            context,
            lowercase_regions: HashMap::new(),
        }
    }

    fn has_region_text(&self, region: &ManifestRegion) -> bool {
        self.context.region_text(region).is_some()
    }

    fn region_text(&self, region: &ManifestRegion) -> Option<&str> {
        self.context.region_text(region)
    }

    fn lowercase_region_text(&mut self, region: &ManifestRegion) -> Option<&str> {
        if !self.lowercase_regions.contains_key(region) {
            let text = self.context.region_text(region)?;
            self.lowercase_regions
                .insert(region.clone(), text.to_lowercase());
        }

        self.lowercase_regions.get(region).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestMatch {
    pub rule_id: String,
    pub activity: AgentActivity,
    pub priority: i32,
    pub region: ManifestRegion,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ManifestRegion {
    OscTitle,
    OscProgress,
    WholeRecent,
    BottomLines(usize),
    BottomNonEmptyLines(usize),
    AfterLastPromptMarker,
    PromptBoxBody,
    AfterLastHorizontalRule,
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
    pub fn with_region_text(mut self, region: ManifestRegion, text: impl Into<String>) -> Self {
        self.regions.insert(region, text.into());
        self
    }

    pub fn region_text(&self, region: &ManifestRegion) -> Option<&str> {
        self.regions.get(region).map(String::as_str)
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to parse manifest TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("manifest source is {size} bytes, max {max}")]
    SourceTooLarge { size: usize, max: usize },
    #[error("manifest contains {count} rules, max {max}")]
    TooManyRules { count: usize, max: usize },
    #[error("manifest contains {count} gates, max {max}")]
    TooManyGates { count: usize, max: usize },
    #[error("manifest contains {count} matchers, max {max}")]
    TooManyMatchers { count: usize, max: usize },
    #[error("manifest gate depth {depth} exceeds max {max}")]
    MaxDepthExceeded { depth: usize, max: usize },
    #[error("rule {rule_id:?} has invalid state {state:?}")]
    InvalidState { rule_id: String, state: String },
    #[error("rule {rule_id:?} has invalid region {region:?}")]
    InvalidRegion { rule_id: String, region: String },
    #[error("rule {rule_id:?} has no gate matchers")]
    MissingGate { rule_id: String },
    #[error("rule {rule_id:?} has invalid regex {pattern:?}: {source}")]
    InvalidRegex {
        rule_id: String,
        pattern: String,
        source: regex::Error,
    },
    #[error("rule {rule_id:?} has {kind:?} matcher of {len} bytes, max {max}")]
    MatcherTooLong {
        rule_id: String,
        kind: MatcherKind,
        len: usize,
        max: usize,
    },
    #[error("rule {rule_id:?} has empty {kind:?} matcher")]
    EmptyMatcher { rule_id: String, kind: MatcherKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherKind {
    Contains,
    Regex,
    LineRegex,
}

#[derive(Debug)]
struct Rule {
    id: String,
    activity: AgentActivity,
    priority: i32,
    region: ManifestRegion,
    gate: Gate,
}

impl Rule {
    fn try_from_raw(raw: RawRule, budget: &mut ComplexityBudget) -> Result<Self, ManifestError> {
        let RawRule {
            id,
            state,
            priority,
            region,
            contains,
            regex,
            line_regex,
            all,
            any,
            not,
            gates,
            visible_blocker: _visible_blocker,
        } = raw;

        let activity = parse_activity(&id, state)?;
        let parsed_region =
            ManifestRegion::from_str(&region).map_err(|()| ManifestError::InvalidRegion {
                rule_id: id.clone(),
                region,
            })?;
        let inline_gate = RawGate {
            contains,
            regex,
            line_regex,
            all,
            any,
            not,
        };
        let gate = compile_rule_gate(&id, inline_gate, gates, budget)?;

        Ok(Self {
            id,
            activity,
            priority,
            region: parsed_region,
            gate,
        })
    }
}

fn parse_activity(rule_id: &str, state: String) -> Result<AgentActivity, ManifestError> {
    match state.as_str() {
        "working" => Ok(AgentActivity::Working),
        "blocked" => Ok(AgentActivity::Blocked),
        "idle" => Ok(AgentActivity::Idle),
        _ => Err(ManifestError::InvalidState {
            rule_id: rule_id.to_string(),
            state,
        }),
    }
}

fn compile_rule_gate(
    rule_id: &str,
    inline_gate: RawGate,
    gates: Option<RawGate>,
    budget: &mut ComplexityBudget,
) -> Result<Gate, ManifestError> {
    let mut parts = collect_gate_parts(rule_id, inline_gate, 1, budget)?;

    if let Some(raw_gate) = gates {
        parts.push(compile_raw_gate(rule_id, raw_gate, 1, budget)?);
    }

    finish_gate_parts(rule_id, parts, 1, budget)
}

fn compile_raw_gate(
    rule_id: &str,
    raw: RawGate,
    depth: usize,
    budget: &mut ComplexityBudget,
) -> Result<Gate, ManifestError> {
    let parts = collect_gate_parts(rule_id, raw, depth, budget)?;
    finish_gate_parts(rule_id, parts, depth, budget)
}

fn collect_gate_parts(
    rule_id: &str,
    raw: RawGate,
    depth: usize,
    budget: &mut ComplexityBudget,
) -> Result<Vec<Gate>, ManifestError> {
    let RawGate {
        contains,
        regex,
        line_regex,
        all,
        any,
        not,
    } = raw;

    let mut parts = Vec::new();
    push_matcher_parts(
        rule_id, contains, regex, line_regex, depth, budget, &mut parts,
    )?;

    if let Some(children) = all {
        if children.is_empty() {
            return Err(ManifestError::MissingGate {
                rule_id: rule_id.to_string(),
            });
        }

        budget.note_gate(depth)?;
        parts.push(Gate::All(compile_child_gates(
            rule_id,
            children,
            depth + 1,
            budget,
        )?));
    }

    if let Some(children) = any {
        if children.is_empty() {
            return Err(ManifestError::MissingGate {
                rule_id: rule_id.to_string(),
            });
        }

        budget.note_gate(depth)?;
        parts.push(Gate::Any(compile_child_gates(
            rule_id,
            children,
            depth + 1,
            budget,
        )?));
    }

    if let Some(child) = not {
        budget.note_gate(depth)?;
        parts.push(Gate::Not(Box::new(compile_raw_gate(
            rule_id,
            *child,
            depth + 1,
            budget,
        )?)));
    }

    Ok(parts)
}

fn compile_child_gates(
    rule_id: &str,
    children: Vec<RawGate>,
    depth: usize,
    budget: &mut ComplexityBudget,
) -> Result<Vec<Gate>, ManifestError> {
    children
        .into_iter()
        .map(|child| compile_raw_gate(rule_id, child, depth, budget))
        .collect()
}

fn push_matcher_parts(
    rule_id: &str,
    contains: Option<MatcherInput>,
    regex: Option<MatcherInput>,
    line_regex: Option<MatcherInput>,
    depth: usize,
    budget: &mut ComplexityBudget,
    parts: &mut Vec<Gate>,
) -> Result<(), ManifestError> {
    if let Some(matchers) = contains {
        for needle in matchers.into_vec() {
            check_matcher_value(rule_id, MatcherKind::Contains, &needle)?;
            budget.note_matcher(depth)?;
            parts.push(Gate::Contains(needle.to_lowercase()));
        }
    }

    if let Some(matchers) = regex {
        for pattern in matchers.into_vec() {
            check_matcher_value(rule_id, MatcherKind::Regex, &pattern)?;
            budget.note_matcher(depth)?;
            parts.push(Gate::Regex(compile_regex(rule_id, pattern)?));
        }
    }

    if let Some(matchers) = line_regex {
        for pattern in matchers.into_vec() {
            check_matcher_value(rule_id, MatcherKind::LineRegex, &pattern)?;
            budget.note_matcher(depth)?;
            parts.push(Gate::LineRegex(compile_regex(rule_id, pattern)?));
        }
    }

    Ok(())
}

fn check_matcher_value(rule_id: &str, kind: MatcherKind, value: &str) -> Result<(), ManifestError> {
    if value.is_empty() {
        return Err(ManifestError::EmptyMatcher {
            rule_id: rule_id.to_string(),
            kind,
        });
    }

    if value.len() > MAX_MATCHER_BYTES {
        return Err(ManifestError::MatcherTooLong {
            rule_id: rule_id.to_string(),
            kind,
            len: value.len(),
            max: MAX_MATCHER_BYTES,
        });
    }

    Ok(())
}

fn compile_regex(rule_id: &str, pattern: String) -> Result<Regex, ManifestError> {
    Regex::new(&pattern).map_err(|source| ManifestError::InvalidRegex {
        rule_id: rule_id.to_string(),
        pattern,
        source,
    })
}

fn finish_gate_parts(
    rule_id: &str,
    parts: Vec<Gate>,
    depth: usize,
    budget: &mut ComplexityBudget,
) -> Result<Gate, ManifestError> {
    match parts.len() {
        0 => Err(ManifestError::MissingGate {
            rule_id: rule_id.to_string(),
        }),
        1 => {
            let gate = parts.into_iter().next().expect("one gate part exists");
            if gate.has_matcher() {
                Ok(gate)
            } else {
                Err(ManifestError::MissingGate {
                    rule_id: rule_id.to_string(),
                })
            }
        }
        _ => {
            if !parts.iter().any(Gate::has_matcher) {
                return Err(ManifestError::MissingGate {
                    rule_id: rule_id.to_string(),
                });
            }

            budget.note_gate(depth)?;
            Ok(Gate::All(parts))
        }
    }
}

#[derive(Debug)]
enum Gate {
    Contains(String),
    Regex(Regex),
    LineRegex(Regex),
    All(Vec<Gate>),
    Any(Vec<Gate>),
    Not(Box<Gate>),
}

impl Gate {
    fn matches(&self, region: &ManifestRegion, evaluation: &mut MatchEvaluation<'_>) -> bool {
        match self {
            Self::Contains(needle) => evaluation
                .lowercase_region_text(region)
                .is_some_and(|text| text.contains(needle)),
            Self::Regex(regex) => evaluation
                .region_text(region)
                .is_some_and(|text| regex.is_match(text)),
            Self::LineRegex(regex) => evaluation
                .region_text(region)
                .is_some_and(|text| text.lines().any(|line| regex.is_match(line))),
            Self::All(gates) => gates.iter().all(|gate| gate.matches(region, evaluation)),
            Self::Any(gates) => gates.iter().any(|gate| gate.matches(region, evaluation)),
            Self::Not(gate) => !gate.matches(region, evaluation),
        }
    }

    fn has_matcher(&self) -> bool {
        match self {
            Self::Contains(_) | Self::Regex(_) | Self::LineRegex(_) => true,
            Self::All(gates) | Self::Any(gates) => gates.iter().any(Gate::has_matcher),
            Self::Not(gate) => gate.has_matcher(),
        }
    }
}

#[derive(Debug, Default)]
struct ComplexityBudget {
    gates: usize,
    matchers: usize,
}

impl ComplexityBudget {
    fn note_gate(&mut self, depth: usize) -> Result<(), ManifestError> {
        check_depth(depth)?;
        self.gates += 1;
        if self.gates > MAX_GATES {
            return Err(ManifestError::TooManyGates {
                count: self.gates,
                max: MAX_GATES,
            });
        }

        Ok(())
    }

    fn note_matcher(&mut self, depth: usize) -> Result<(), ManifestError> {
        check_depth(depth)?;
        self.matchers += 1;
        if self.matchers > MAX_MATCHERS {
            return Err(ManifestError::TooManyMatchers {
                count: self.matchers,
                max: MAX_MATCHERS,
            });
        }

        Ok(())
    }
}

fn check_depth(depth: usize) -> Result<(), ManifestError> {
    if depth > MAX_DEPTH {
        return Err(ManifestError::MaxDepthExceeded {
            depth,
            max: MAX_DEPTH,
        });
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    state: String,
    priority: i32,
    region: String,
    contains: Option<MatcherInput>,
    regex: Option<MatcherInput>,
    line_regex: Option<MatcherInput>,
    all: Option<Vec<RawGate>>,
    any: Option<Vec<RawGate>>,
    not: Option<Box<RawGate>>,
    gates: Option<RawGate>,
    // Parsed for schema compatibility with milestone manifests. The current
    // matcher API only emits the winning activity and source metadata, so this
    // flag stays out of ManifestMatch until detector integration has a consumer.
    visible_blocker: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGate {
    contains: Option<MatcherInput>,
    regex: Option<MatcherInput>,
    line_regex: Option<MatcherInput>,
    all: Option<Vec<RawGate>>,
    any: Option<Vec<RawGate>>,
    not: Option<Box<RawGate>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MatcherInput {
    One(String),
    Many(Vec<String>),
}

impl MatcherInput {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::AgentActivity;

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
            })
        );
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
    fn empty_all_gate_is_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "empty-all"
            state = "working"
            priority = 1
            region = "whole_recent"
            all = []
            "#,
        )
        .expect_err("empty all gate should be rejected");

        assert!(matches!(
            error,
            ManifestError::MissingGate { rule_id } if rule_id == "empty-all"
        ));
    }

    #[test]
    fn empty_any_gate_is_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "empty-any"
            state = "working"
            priority = 1
            region = "whole_recent"
            any = []
            "#,
        )
        .expect_err("empty any gate should be rejected");

        assert!(matches!(
            error,
            ManifestError::MissingGate { rule_id } if rule_id == "empty-any"
        ));
    }

    #[test]
    fn not_wrapping_empty_any_gate_is_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "not-empty-any"
            state = "working"
            priority = 1
            region = "whole_recent"
            not = { any = [] }
            "#,
        )
        .expect_err("not wrapping empty any should be rejected");

        assert!(matches!(
            error,
            ManifestError::MissingGate { rule_id } if rule_id == "not-empty-any"
        ));
    }

    #[test]
    fn empty_contains_matcher_is_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "empty-contains"
            state = "working"
            priority = 1
            region = "whole_recent"
            contains = ""
            "#,
        )
        .expect_err("empty contains matcher should be rejected");

        assert!(matches!(
            error,
            ManifestError::EmptyMatcher {
                rule_id,
                kind: MatcherKind::Contains,
            } if rule_id == "empty-contains"
        ));
    }

    #[test]
    fn empty_regex_matcher_is_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "empty-regex"
            state = "working"
            priority = 1
            region = "whole_recent"
            regex = ""
            "#,
        )
        .expect_err("empty regex matcher should be rejected");

        assert!(matches!(
            error,
            ManifestError::EmptyMatcher {
                rule_id,
                kind: MatcherKind::Regex,
            } if rule_id == "empty-regex"
        ));
    }

    #[test]
    fn empty_line_regex_matcher_is_rejected() {
        let error = Manifest::parse_str(
            r#"
            [[rules]]
            id = "empty-line-regex"
            state = "working"
            priority = 1
            region = "whole_recent"
            line_regex = ""
            "#,
        )
        .expect_err("empty line regex matcher should be rejected");

        assert!(matches!(
            error,
            ManifestError::EmptyMatcher {
                rule_id,
                kind: MatcherKind::LineRegex,
            } if rule_id == "empty-line-regex"
        ));
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

        assert_eq!(
            manifest
                .match_context(&context)
                .map(|matched| matched.rule_id),
            Some("live_blocked_form".to_string())
        );
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
