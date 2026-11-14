//! The onboarding guide's floor against its Claude deny table.
//!
//! Each floor bullet names commands as backticked `pohunek ...` spans (words
//! joined by `|` are alternatives) and flags as backticked `--flag` spans. A
//! flag belongs to the nearest command before it in the bullet, else to the
//! first one after it; in a bullet without a command it belongs to every
//! command (`pohunek` alone). Every floor command needs an `always` deny rule
//! for that command, and every floor flag an `always` rule for its command
//! whose pattern carries the flag, unless [`FLOOR_FLAGS_WITHOUT_RULE`] lists
//! it. Every `Bash(pohunek ...)` deny rule names a floor command, or is listed
//! in [`DENY_RULES_BEYOND_FLOOR`]. A rule's command is its words before the
//! first one that holds `*` or starts with `-`. Both lists carry a reason per
//! entry, and an entry that no longer matches the guide fails the check.

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

/// Start of a floor bullet; its indented lines after it continue it.
const BULLET: &str = "- ";

/// Start of the deny table's header row; the table ends at the first line
/// that is not a table row.
const DENY_TABLE_HEADER: &str = "| Deny rule |";

/// *Include when* value of a rule the launch profile always carries.
const ALWAYS: &str = "always";

/// Prefix of the deny rules this check pairs with floor commands.
const POHUNEK_RULE_PREFIX: &str = "Bash(pohunek ";

/// The command a floor flag outside any command's bullet applies to.
const ANY_COMMAND: &str = "pohunek";

/// Floor flags no deny rule can carry, as (command, flag, reason).
const FLOOR_FLAGS_WITHOUT_RULE: [(&str, &str, &str); 1] = [(
    "pohunek integration install",
    "--profile",
    "the floor refuses the command without the flag, which no text rule can single out",
)];

/// `Bash(pohunek ...)` deny rules that do not refuse a floor command, as
/// (rule, reason).
const DENY_RULES_BEYOND_FLOOR: [(&str, &str); 5] = [
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

/// One command or one command's flag the floor forbids.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FloorItem {
    command: Vec<String>,
    flag: Option<String>,
}

/// One `Bash(pohunek ...)` row of the deny table.
struct DenyRule {
    rule: String,
    include: String,
    command: Vec<String>,
    pattern: String,
}

impl DenyRule {
    /// Whether the rule's pattern holds `flag` as a whole flag name.
    fn carries(&self, flag: &str) -> bool {
        self.pattern.match_indices(flag).any(|(start, _)| {
            self.pattern[start + flag.len()..]
                .chars()
                .next()
                .is_none_or(|next| !(next.is_ascii_alphanumeric() || next == '-'))
        })
    }
}

/// The bullets of the floor section, each with its lines joined by spaces so
/// a backticked span wrapped across lines stays whole; `None` without the
/// heading.
fn floor_bullets(content: &str) -> Option<Vec<String>> {
    let mut lines = content
        .lines()
        .skip_while(|line| line.trim() != FLOOR_HEADING);
    lines.next()?;
    let mut bullets: Vec<String> = Vec::new();
    for line in lines.take_while(|line| !line.starts_with('#')) {
        if let Some(start) = line.strip_prefix(BULLET) {
            bullets.push(start.to_owned());
        } else if line.starts_with(' ') {
            if let Some(bullet) = bullets.last_mut() {
                bullet.push(' ');
                bullet.push_str(line.trim());
            }
        }
    }
    Some(bullets)
}

/// What one bullet's backticked span names.
enum Span {
    Commands(Vec<Vec<String>>),
    Flag(String),
}

/// The items one floor bullet forbids.
fn bullet_items(bullet: &str) -> Vec<FloorItem> {
    let span = Regex::new(r"`([^`]+)`").expect("valid span regex");
    let spans: Vec<Span> = span
        .captures_iter(bullet)
        .filter_map(|captures| {
            let text = &captures[1];
            if text.starts_with("pohunek ") {
                let tokens: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
                Some(Span::Commands(expand_alternatives(&tokens)))
            } else if text.starts_with("--") && !text.contains(char::is_whitespace) {
                Some(Span::Flag(text.to_owned()))
            } else {
                None
            }
        })
        .collect();
    let commands_at = |index: usize| match &spans[index] {
        Span::Commands(commands) => Some(commands.clone()),
        Span::Flag(_) => None,
    };
    let mut items = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        match span {
            Span::Commands(commands) => items.extend(commands.iter().map(|command| FloorItem {
                command: command.clone(),
                flag: None,
            })),
            Span::Flag(flag) => {
                let owners = (0..index)
                    .rev()
                    .find_map(commands_at)
                    .or_else(|| (index + 1..spans.len()).find_map(commands_at))
                    .unwrap_or_else(|| vec![vec![ANY_COMMAND.to_owned()]]);
                items.extend(owners.into_iter().map(|command| FloorItem {
                    command,
                    flag: Some(flag.clone()),
                }));
            }
        }
    }
    items
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
                pattern: inner.to_owned(),
            })
        })
        .collect();
    Some(rules)
}

/// Whether [`FLOOR_FLAGS_WITHOUT_RULE`] lists `item`.
fn needs_no_rule(item: &FloorItem) -> bool {
    FLOOR_FLAGS_WITHOUT_RULE.iter().any(|(command, flag, _)| {
        item.command.join(" ") == *command && item.flag.as_deref() == Some(*flag)
    })
}

/// The problems of `content` as the onboarding guide, one line each.
fn problems(content: &str) -> Vec<String> {
    let Some(bullets) = floor_bullets(content) else {
        return vec![format!("no `{FLOOR_HEADING}` section")];
    };
    let Some(rules) = deny_rules(content) else {
        return vec![format!("no table starting `{DENY_TABLE_HEADER}`")];
    };
    let items: BTreeSet<FloorItem> = bullets.iter().flat_map(|b| bullet_items(b)).collect();
    let mut problems = Vec::new();
    for item in items.iter().filter(|item| !needs_no_rule(item)) {
        let refused = rules.iter().any(|rule| {
            rule.command == item.command
                && rule.include == ALWAYS
                && item.flag.as_deref().is_none_or(|flag| rule.carries(flag))
        });
        if !refused {
            let command = item.command.join(" ");
            problems.push(match &item.flag {
                None => format!("floor command without an `{ALWAYS}` deny rule: {command}"),
                Some(flag) => {
                    format!("floor flag without an `{ALWAYS}` deny rule: {command} {flag}")
                }
            });
        }
    }
    let commands: BTreeSet<&Vec<String>> = items.iter().map(|item| &item.command).collect();
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
    for (command, flag, _) in FLOOR_FLAGS_WITHOUT_RULE {
        let named = items
            .iter()
            .any(|item| item.command.join(" ") == command && item.flag.as_deref() == Some(flag));
        if !named {
            problems.push(format!(
                "listed as a floor flag without a rule, not in the floor: {command} {flag}"
            ));
        }
    }
    problems
}

/// The onboarding guide's floor commands and flags and its
/// `Bash(pohunek ...)` deny rules match.
pub(super) fn check(repo: &Path) -> Result<bool, XtaskError> {
    let content = read_page(repo, Path::new(ONBOARDING_GUIDE))?;
    let problems = problems(&content);
    if problems.is_empty() {
        println!(
            "[PASS] onboarding-floor: {ONBOARDING_GUIDE} has a deny rule for every floor command and flag"
        );
        return Ok(true);
    }
    println!("[FAIL] onboarding-floor: {ONBOARDING_GUIDE} floor and deny table differ:");
    for problem in &problems {
        println!("        {problem}");
    }
    Ok(false)
}
