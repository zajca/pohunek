//! Shared provider prompt rendering.
//!
//! This crate renders daemon-resolved prompt templates for provider work items.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-07-19

pub mod link;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy)]
struct Field {
    names: &'static [&'static str],
    label: &'static str,
    required: bool,
}

const TITLE_FIELD: Field = Field {
    names: &["title"],
    label: "title",
    required: true,
};
const GITHUB_BODY_FIELD: Field = Field {
    names: &["body", "description"],
    label: "body/description",
    required: false,
};
const GITHUB_BRANCH_FIELD: Field = Field {
    names: &["headRefName", "branch", "branchName"],
    label: "headRefName/branch/branchName",
    required: true,
};
const LINEAR_ID_FIELD: Field = Field {
    names: &["identifier", "id"],
    label: "identifier/id",
    required: false,
};
const LINEAR_BODY_FIELD: Field = Field {
    names: &["description", "body"],
    label: "description/body",
    required: false,
};
const LINEAR_BRANCH_FIELD: Field = Field {
    names: &["branchName", "branch"],
    label: "branchName/branch",
    required: true,
};
const URL_FIELD: Field = Field {
    names: &["url"],
    label: "url",
    required: false,
};
const REVIEW_BRANCH_FIELD: Field = Field {
    names: &["branch"],
    label: "branch",
    required: true,
};
const REVIEW_SOURCE_FIELD: Field = Field {
    names: &["source"],
    label: "source",
    required: true,
};
const REVIEW_COMMENTS_FIELD: Field = Field {
    names: &["comments"],
    label: "comments",
    required: false,
};
/// JSON key for the review comment count. Not a [`Field`]/[`pick`] constant
/// because, unlike every other field, it accepts either a JSON integer or a
/// JSON string (see [`pick_comment_count`]).
const REVIEW_COMMENT_COUNT_KEY: &str = "comment_count";

/// Provider context supported by the shared prompt renderer.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Provider {
    /// A Linear issue JSON object.
    LinearIssue,
    /// A GitHub pull request JSON object.
    GitHubPr,
    /// A review draft built by gui-core from typed `Review`/`ReviewComment` data.
    ///
    /// Context keys: `${provider}` (always `"review"`), `${branch}`, `${source}`
    /// (a human description of what was reviewed, e.g. `"PR #42"` or `"session
    /// abc12345 worktree diff"`), `${comments}` (pre-rendered `path:line (side):
    /// text` blocks, empty string when there are none), and `${comment_count}`.
    /// Unlike the other providers, gui-core is the sole producer of this JSON,
    /// so field extraction uses direct single-name lookups rather than the
    /// multi-name aliasing [`pick`] exists for.
    Review,
}

impl Provider {
    /// Returns the stable provider label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinearIssue => "linear_issue",
            Self::GitHubPr => "github_pr",
            Self::Review => "review",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "linear_issue" => Ok(Self::LinearIssue),
            "github_pr" => Ok(Self::GitHubPr),
            "review" => Ok(Self::Review),
            other => Err(Error::UnknownProvider(other.to_owned())),
        }
    }
}

/// Prompt rendering error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Provider JSON was not valid JSON.
    #[error("provider returned invalid JSON: {0}")]
    InvalidJson(serde_json::Error),
    /// The provider label is not supported.
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    /// The provider JSON did not contain a required field.
    #[error("provider JSON missing required field: {0}")]
    MissingRequiredField(&'static str),
    /// A session link metadata field was empty.
    #[error("provider link metadata is missing `{field}`")]
    MissingLinkField { field: &'static str },
    /// A session link metadata field contained invalid bytes.
    #[error("provider link metadata `{field}` contains an ASCII control character")]
    InvalidLinkField { field: &'static str },
    /// The template referenced variables outside the provider context.
    #[error("template references unknown variable(s): {}", .0.join(", "))]
    UnknownVariables(Vec<String>),
    /// The provider does not support deriving session-link metadata.
    #[error(
        "provider `{0}` does not support link metadata derivation; \
         review sessions copy `link.*` keys from their source session instead"
    )]
    LinkUnsupportedProvider(&'static str),
}

/// Renders a provider prompt template.
///
/// Substitution is single-pass: provider values containing `${name}` are copied
/// literally and are never expanded again. See each [`Provider`] variant's docs
/// for the `${var}` names its context provides; for [`Provider::Review`],
/// `item_id` is not part of the rendered context — pass the review id there for
/// error context only, since the review context is self-contained.
///
/// # Errors
///
/// Returns an error when the provider JSON is invalid, the provider-specific
/// required fields are missing, or the template references unknown variables.
pub fn render(
    template: impl AsRef<str>,
    provider: Provider,
    item_id: impl AsRef<str>,
    raw_json: impl AsRef<str>,
) -> Result<String, Error> {
    let data: serde_json::Value =
        serde_json::from_str(raw_json.as_ref()).map_err(Error::InvalidJson)?;
    let context = build_context(provider, item_id.as_ref(), &data)?;
    validate_variables(template.as_ref(), &context)?;
    Ok(substitute(template.as_ref(), &context))
}

/// Renders a static prompt template.
///
/// Static templates have no provider context. Any `${var}` reference is rejected
/// so callers do not accidentally launch a partially-rendered prompt.
///
/// # Errors
///
/// Returns an error when the template references any variable.
pub fn render_static(template: impl AsRef<str>) -> Result<String, Error> {
    let context = BTreeMap::new();
    validate_variables(template.as_ref(), &context)?;
    Ok(substitute(template.as_ref(), &context))
}

fn build_context(
    provider: Provider,
    item_id: &str,
    data: &serde_json::Value,
) -> Result<BTreeMap<&'static str, String>, Error> {
    let mut context = BTreeMap::new();

    match provider {
        Provider::GitHubPr => {
            context.insert("provider", "github".to_owned());
            context.insert("number", item_id.to_owned());
            context.insert("id", item_id.to_owned());
            context.insert("title", pick(data, TITLE_FIELD)?);
            context.insert("body", pick(data, GITHUB_BODY_FIELD)?);
            context.insert("branch", pick(data, GITHUB_BRANCH_FIELD)?);
            context.insert("url", pick(data, URL_FIELD)?);
        }
        Provider::LinearIssue => {
            context.insert("provider", "linear".to_owned());
            let id = pick(data, LINEAR_ID_FIELD)?;
            let id = if id.is_empty() {
                item_id.to_owned()
            } else {
                id
            };
            context.insert("id", id.clone());
            context.insert("number", id);
            context.insert("title", pick(data, TITLE_FIELD)?);
            context.insert("body", pick(data, LINEAR_BODY_FIELD)?);
            context.insert("branch", pick(data, LINEAR_BRANCH_FIELD)?);
            context.insert("url", pick(data, URL_FIELD)?);
        }
        Provider::Review => {
            // `item_id` is not part of this context: gui-core builds the review
            // context directly from typed data, so callers pass the review id as
            // `item_id` for error/log context only, not for substitution.
            context.insert("provider", "review".to_owned());
            context.insert("branch", pick(data, REVIEW_BRANCH_FIELD)?);
            context.insert("source", pick(data, REVIEW_SOURCE_FIELD)?);
            context.insert("comments", pick(data, REVIEW_COMMENTS_FIELD)?);
            context.insert("comment_count", pick_comment_count(data)?);
        }
    }

    Ok(context)
}

/// Extracts the review comment count, accepted as a JSON integer or a
/// non-empty JSON string so gui-core's context builder can send either.
fn pick_comment_count(data: &serde_json::Value) -> Result<String, Error> {
    match data.get(REVIEW_COMMENT_COUNT_KEY) {
        Some(serde_json::Value::Number(count)) => Ok(count.to_string()),
        Some(serde_json::Value::String(count)) if !count.is_empty() => Ok(count.clone()),
        _ => Err(Error::MissingRequiredField("comment_count")),
    }
}

fn pick(data: &serde_json::Value, field: Field) -> Result<String, Error> {
    for name in field.names {
        if let Some(value) = data.get(*name).and_then(serde_json::Value::as_str) {
            if !value.is_empty() {
                return Ok(value.to_owned());
            }
        }
    }

    if field.required {
        Err(Error::MissingRequiredField(field.label))
    } else {
        Ok(String::new())
    }
}

fn validate_variables(
    template: &str,
    context: &BTreeMap<&'static str, String>,
) -> Result<(), Error> {
    let unknown: Vec<String> = placeholders(template)
        .into_iter()
        .filter(|name| !context.contains_key(name.as_str()))
        .collect();

    if unknown.is_empty() {
        Ok(())
    } else {
        Err(Error::UnknownVariables(unknown))
    }
}

fn placeholders(template: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    let mut rest = template;

    while let Some(start) = rest.find("${") {
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            break;
        };
        let name = &after_start[..end];
        if is_variable_name(name) {
            names.insert(name.to_owned());
        }
        rest = &after_start[end + 1..];
    }

    names.into_iter().collect()
}

fn substitute(template: &str, context: &BTreeMap<&'static str, String>) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("${") {
        rendered.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            rendered.push_str(&rest[start..]);
            return rendered;
        };
        let name = &after_start[..end];
        if is_variable_name(name) {
            if let Some(value) = context.get(name) {
                rendered.push_str(value);
            } else {
                rendered.push_str("${");
                rendered.push_str(name);
                rendered.push('}');
            }
        } else {
            rendered.push_str("${");
            rendered.push_str(name);
            rendered.push('}');
        }
        rest = &after_start[end + 1..];
    }

    rendered.push_str(rest);
    rendered
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::{render, render_static, Error, Provider};

    #[test]
    fn static_render_accepts_literal_template() {
        let rendered = render_static("Run the static checklist\n").expect("static render");

        assert_eq!(rendered, "Run the static checklist\n");
    }

    #[test]
    fn static_render_rejects_unknown_variables() {
        let err = render_static("Issue ${title}").expect_err("unknown variable");

        assert!(matches!(err, Error::UnknownVariables(names) if names == vec!["title"]));
    }

    #[test]
    fn provider_from_str_and_as_str_round_trip_includes_review() {
        assert_eq!(Provider::Review.as_str(), "review");
        assert_eq!(
            Provider::from_str("review").expect("parse review provider"),
            Provider::Review
        );
    }

    #[test]
    fn render_review_golden_produces_expected_prompt_text() {
        let template = "Review of ${source} on branch ${branch} (${comment_count} comments):\n\
                         ${comments}\n";
        let context_json = r#"{
            "branch": "feature/diff-review",
            "source": "PR #42",
            "comments": "src/lib.rs:10 (new): fix this\nsrc/lib.rs:20 (old): remove dead code",
            "comment_count": 2
        }"#;

        let rendered = render(template, Provider::Review, "review-abc123", context_json)
            .expect("review render");

        assert_eq!(
            rendered,
            "Review of PR #42 on branch feature/diff-review (2 comments):\n\
             src/lib.rs:10 (new): fix this\nsrc/lib.rs:20 (old): remove dead code\n"
        );
    }

    #[test]
    fn render_review_accepts_comment_count_as_json_string() {
        let context_json = r#"{"branch":"b","source":"s","comment_count":"3"}"#;

        let rendered = render("${comment_count}", Provider::Review, "id", context_json)
            .expect("review render with string comment_count");

        assert_eq!(rendered, "3");
    }

    #[test]
    fn render_review_treats_missing_comments_as_empty_string() {
        let context_json = r#"{"branch":"b","source":"s","comment_count":0}"#;

        let rendered = render("[${comments}]", Provider::Review, "id", context_json)
            .expect("review render with no comments");

        assert_eq!(rendered, "[]");
    }

    #[test]
    fn render_review_rejects_unknown_variables() {
        let context_json = r#"{"branch":"b","source":"s","comment_count":0}"#;

        let err = render("${nope}", Provider::Review, "id", context_json)
            .expect_err("unknown variable rejected");

        assert!(matches!(err, Error::UnknownVariables(names) if names == vec!["nope"]));
    }

    #[test]
    fn render_review_missing_branch_errors() {
        let context_json = r#"{"source":"s","comment_count":0}"#;

        let err = render("${branch}", Provider::Review, "id", context_json)
            .expect_err("missing branch rejected");

        assert!(matches!(err, Error::MissingRequiredField("branch"),));
    }

    #[test]
    fn render_review_missing_comment_count_errors() {
        let context_json = r#"{"branch":"b","source":"s"}"#;

        let err = render("${comment_count}", Provider::Review, "id", context_json)
            .expect_err("missing comment_count rejected");

        assert!(matches!(err, Error::MissingRequiredField("comment_count"),));
    }
}
