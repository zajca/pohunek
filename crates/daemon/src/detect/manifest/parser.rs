use std::str::FromStr;

use protocol::AgentActivity;
use regex::Regex;
use serde::Deserialize;

use super::error::{ManifestError, MatcherKind};
use super::matcher::Gate;
use super::{ManifestRegion, ProcessMatchers};

/// Maximum number of `[[rules]]` entries a manifest may declare.
///
/// Manifests can be host-supplied profile overrides parsed at the trust
/// boundary, so the parser caps total complexity to keep a hostile or buggy
/// manifest from blowing up memory or per-frame match cost. This bounds the
/// outer rule list; nested gate/matcher fan-out is bounded separately below.
pub(super) const MAX_RULES: usize = 128;
/// Maximum number of compiled boolean gate nodes (`all`/`any`/`not`) across the
/// whole manifest. Caps the size of the boolean tree the matcher walks every
/// frame so deeply combinatorial manifests cannot make detection quadratic.
pub(super) const MAX_GATES: usize = 512;
/// Maximum number of leaf matchers (`contains`/`regex`/`line_regex`) across the
/// whole manifest. Each leaf is a string scan or regex run per frame, so this
/// bounds total per-frame matching work independently of gate nesting.
pub(super) const MAX_MATCHERS: usize = 1024;
/// Maximum gate-nesting depth. Bounds recursion in both the compiler and the
/// per-frame matcher so a pathologically nested manifest cannot overflow the
/// stack; 8 is far deeper than any hand-written rule needs.
pub(super) const MAX_DEPTH: usize = 8;
/// Maximum manifest source size in bytes (256 KiB). Rejected before TOML
/// parsing so an oversized host-supplied manifest cannot exhaust memory in the
/// deserializer; real manifests are a few KiB.
pub(super) const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// Maximum byte length of a single matcher string (4 KiB). Keeps one
/// `contains` needle or `regex`/`line_regex` pattern from being absurdly large
/// (regex compile/run cost scales with pattern size); real matchers are short.
pub(super) const MAX_MATCHER_BYTES: usize = 4096;
/// Synthetic rule id used in errors for `[process].comm` matchers.
const PROCESS_COMM_MATCHER_ID: &str = "process.comm";
/// Synthetic rule id used in errors for `[process].cmdline` matchers.
const PROCESS_CMDLINE_MATCHER_ID: &str = "process.cmdline";

#[derive(Debug, Clone)]
pub(super) struct Rule {
    pub(super) id: String,
    pub(super) activity: AgentActivity,
    pub(super) priority: i32,
    pub(super) region: ManifestRegion,
    pub(super) visible_blocker: bool,
    pub(super) gate: Gate,
}

impl Rule {
    pub(super) fn try_from_raw(
        raw: RawRule,
        budget: &mut ComplexityBudget,
    ) -> Result<Self, ManifestError> {
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
            visible_blocker,
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
            visible_blocker: visible_blocker.unwrap_or(false),
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

pub(super) fn compile_process_matchers(
    raw: RawProcessSection,
    budget: &mut ComplexityBudget,
) -> Result<ProcessMatchers, ManifestError> {
    let RawProcessSection { comm, cmdline } = raw;
    Ok(ProcessMatchers::new(
        compile_process_regexes(PROCESS_COMM_MATCHER_ID, comm, budget)?,
        compile_process_regexes(PROCESS_CMDLINE_MATCHER_ID, cmdline, budget)?,
    ))
}

fn compile_process_regexes(
    rule_id: &str,
    input: Option<MatcherInput>,
    budget: &mut ComplexityBudget,
) -> Result<Vec<Regex>, ManifestError> {
    let Some(input) = input else {
        return Ok(Vec::new());
    };
    let mut regexes = Vec::new();
    for pattern in input.into_vec() {
        check_matcher_value(rule_id, MatcherKind::Regex, &pattern)?;
        budget.note_matcher(1)?;
        regexes.push(compile_regex(rule_id, pattern)?);
    }
    Ok(regexes)
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

#[derive(Debug, Default)]
pub(super) struct ComplexityBudget {
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
pub(super) struct RawManifest {
    #[serde(default)]
    pub(super) rules: Vec<RawRule>,
    pub(super) process: Option<RawProcessSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProcessSection {
    comm: Option<MatcherInput>,
    cmdline: Option<MatcherInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRule {
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
