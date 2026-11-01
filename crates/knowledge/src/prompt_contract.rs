//! Shape of the assistant prompt contract, `assistant/system.md`.
//!
//! The `## Safety` and `## Navigation` sections are rendered verbatim, one line
//! per item, into the opening prompt. Bundle validation and the launch parse
//! them with the same rules.

use thiserror::Error;

/// Bundle-relative path of the file that defines the prompt's safety rules and
/// navigation steps.
pub const PROMPT_CONTRACT_PATH: &str = "assistant/system.md";

const SAFETY_HEADING: &str = "Safety";
const NAVIGATION_HEADING: &str = "Navigation";

/// Safety rules and navigation steps of the opening prompt, one line each.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptContract {
    /// Items of the `## Safety` section.
    pub safety: Vec<String>,
    /// Items of the `## Navigation` section.
    pub navigation: Vec<String>,
}

/// A prompt contract that does not have the required shape.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("{}{reason}", line.map_or_else(String::new, |line| format!("line {line}: ")))]
pub struct PromptContractError {
    line: Option<usize>,
    reason: String,
}

impl PromptContractError {
    /// One-based line of the offending text, absent when the contract as a
    /// whole is at fault.
    #[must_use]
    pub fn line(&self) -> Option<usize> {
        self.line
    }

    /// What is wrong with the contract.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn at(line: usize, reason: String) -> Self {
        Self {
            line: Some(line),
            reason,
        }
    }
}

impl PromptContract {
    /// Parse the contract text.
    ///
    /// Each of the `## Safety` and `## Navigation` sections holds only
    /// top-level `- ` bullets or `N. ` steps, each followed by indented
    /// continuation lines, and at least one item.
    ///
    /// # Errors
    ///
    /// Returns a [`PromptContractError`] naming the first line that is prose, a
    /// sub-heading, a nested or `*` list item or an orphan continuation line,
    /// or the section that has no item.
    pub fn parse(text: &str) -> Result<Self, PromptContractError> {
        let safety = section_items(text, SAFETY_HEADING)?;
        let navigation = section_items(text, NAVIGATION_HEADING)?;
        for (heading, items) in [(SAFETY_HEADING, &safety), (NAVIGATION_HEADING, &navigation)] {
            if items.is_empty() {
                return Err(PromptContractError {
                    line: None,
                    reason: format!("the `{heading}` section has no list item"),
                });
            }
        }
        Ok(Self { safety, navigation })
    }
}

/// Items of the `## <heading>` section of `text`: each top-level `- ` bullet or
/// `N. ` step, with its continuation lines (indented by spaces or tabs) joined by single spaces.
///
/// Any other content in the section (prose, sub-headings, nested or `*`
/// bullets) is refused, so an edit of the contract cannot be merged into a
/// neighbouring rule unnoticed.
fn section_items(text: &str, heading: &str) -> Result<Vec<String>, PromptContractError> {
    let mut items: Vec<String> = Vec::new();
    let mut in_section = false;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if let Some(title) = line.strip_prefix("## ") {
            in_section = title.trim() == heading;
            continue;
        }
        if !in_section || line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim();
        if !line.starts_with([' ', '\t']) {
            let item = list_item(trimmed).ok_or_else(|| {
                PromptContractError::at(
                    number,
                    format!("`{heading}` holds a line that is not a `- ` or `N. ` list item"),
                )
            })?;
            items.push(item.to_owned());
        } else if list_item(trimmed).is_some() || trimmed.starts_with(['*', '#']) {
            return Err(PromptContractError::at(
                number,
                format!("`{heading}` holds a nested list or heading"),
            ));
        } else if let Some(last) = items.last_mut() {
            last.push(' ');
            last.push_str(trimmed);
        } else {
            return Err(PromptContractError::at(
                number,
                format!("`{heading}` starts with a continuation line"),
            ));
        }
    }
    Ok(items)
}

/// The text after a `- ` bullet marker or an `N. ` step number.
fn list_item(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ") {
        return Some(rest);
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..].strip_prefix(". ")
}
