//! The onboarding guide's floor against its Claude deny table.
//!
//! Every `pohunek ...` command the floor names (words joined by `|` are
//! alternatives) needs a deny rule for that command with `always` in its
//! *Include when* cell. Every `Bash(pohunek ...)` deny rule names a floor
//! command, or is listed in [`DENY_RULES_BEYOND_FLOOR`] with its reason. A
//! rule's command is its words before the first one that holds `*` or starts
//! with `-`.

// Rust guideline compliant 2026-11-14

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;

use super::{expand_alternatives, read_page};
use crate::XtaskError;

/// The guide whose floor and deny table are compared.
const ONBOARDING_GUIDE: &str = "docs/knowledge/guides/onboarding.md";

/// Heading of the floor section; it ends at the next heading.
const FLOOR_HEADING: &str = "### Floor";

/// Start of the deny table's header row; the table ends at the first line
/// that is not a table row.
const DENY_TABLE_HEADER: &str = "| Deny rule |";

/// *Include when* value of a rule the launch profile always carries.
const ALWAYS: &str = "always";

/// Prefix of the deny rules this check pairs with floor commands.
const POHUNEK_RULE_PREFIX: &str = "Bash(pohunek ";

/// `Bash(pohunek ...)` deny rules that do not refuse one floor command. Each
/// entry needs a reason, and each must still be in the table.
const DENY_RULES_BEYOND_FLOOR: [(&str, &str); 6] = [
    (
        "Bash(pohunek *--host*)",
        "the floor's `--host` item applies to every command",
    ),
    (
        "Bash(pohunek session metadata */*)",
        "the floor's host-qualified target item, inside the `groups` grant's command",
    ),
    (
        "Bash(pohunek project candidates *history*)",
        "history consent, included only while `onboard.history` is not `yes`",
    ),
    (
        "Bash(pohunek plugin profile migrate *--digest*)",
        "narrows the `profiles` grant's allow rule",
    ),
    (
        "Bash(pohunek session metadata *--clear*)",
        "narrows the `groups` grant's allow rule",
    ),
    (
        "Bash(pohunek session metadata *--set*--set*)",
        "narrows the `groups` grant's allow rule",
    ),
];

/// One `Bash(pohunek ...)` row of the deny table.
struct DenyRule {
    rule: String,
    include: String,
    command: Vec<String>,
}

/// The text of the floor section, its lines joined by spaces so a backticked
/// span wrapped across lines stays whole; `None` without the heading.
fn floor_section(content: &str) -> Option<String> {
    let mut lines = content
        .lines()
        .skip_while(|line| line.trim() != FLOOR_HEADING);
    lines.next()?;
    let section: Vec<&str> = lines.take_while(|line| !line.starts_with('#')).collect();
    Some(section.join(" "))
}

/// The commands the floor names: each backticked `pohunek ...` span, one per
/// alternative.
fn floor_commands(floor: &str) -> BTreeSet<Vec<String>> {
    let span = Regex::new(r"`([^`]+)`").expect("valid span regex");
    span.captures_iter(floor)
        .map(|captures| captures[1].to_owned())
        .filter(|text| text.starts_with("pohunek "))
        .flat_map(|text| {
            let tokens: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
            expand_alternatives(&tokens)
        })
        .collect()
}

/// The `Bash(pohunek ...)` rows of the deny table; `None` without the table.
fn deny_rules(content: &str) -> Option<Vec<DenyRule>> {
    let mut lines = content
        .lines()
        .skip_while(|line| !line.starts_with(DENY_TABLE_HEADER));
    lines.next()?;
    let rules = lines
        .take_while(|line| line.starts_with('|'))
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let rule = cells.get(1)?.trim_matches('`');
            let include = cells.get(2)?;
            let inner = rule.strip_prefix("Bash(")?.strip_suffix(')')?;
            rule.starts_with(POHUNEK_RULE_PREFIX).then(|| DenyRule {
                rule: rule.to_owned(),
                include: (*include).to_owned(),
                command: inner
                    .split_whitespace()
                    .take_while(|word| !word.contains('*') && !word.starts_with('-'))
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect();
    Some(rules)
}

/// The problems of `content` as the onboarding guide, one line each.
fn problems(content: &str) -> Vec<String> {
    let Some(floor) = floor_section(content) else {
        return vec![format!("no `{FLOOR_HEADING}` section")];
    };
    let Some(rules) = deny_rules(content) else {
        return vec![format!("no table starting `{DENY_TABLE_HEADER}`")];
    };
    let commands = floor_commands(&floor);
    let mut problems = Vec::new();
    for command in &commands {
        let refused = rules
            .iter()
            .any(|rule| &rule.command == command && rule.include == ALWAYS);
        if !refused {
            problems.push(format!(
                "floor command without an `{ALWAYS}` deny rule: {}",
                command.join(" ")
            ));
        }
    }
    for rule in &rules {
        let exempt = DENY_RULES_BEYOND_FLOOR
            .iter()
            .any(|(listed, _)| *listed == rule.rule);
        if !exempt && !commands.contains(&rule.command) {
            problems.push(format!("deny rule without a floor command: {}", rule.rule));
        }
    }
    for (listed, _) in DENY_RULES_BEYOND_FLOOR {
        if !rules.iter().any(|rule| rule.rule == listed) {
            problems.push(format!(
                "listed as beyond the floor, not in the deny table: {listed}"
            ));
        }
    }
    problems
}

/// The onboarding guide's floor commands and its `Bash(pohunek ...)` deny
/// rules match.
pub(super) fn check(repo: &Path) -> Result<bool, XtaskError> {
    let content = read_page(repo, Path::new(ONBOARDING_GUIDE))?;
    let problems = problems(&content);
    if problems.is_empty() {
        println!(
            "[PASS] onboarding-floor: {ONBOARDING_GUIDE} has a deny rule for every floor command"
        );
        return Ok(true);
    }
    println!("[FAIL] onboarding-floor: {ONBOARDING_GUIDE} floor and deny table differ:");
    for problem in &problems {
        println!("        {problem}");
    }
    Ok(false)
}
