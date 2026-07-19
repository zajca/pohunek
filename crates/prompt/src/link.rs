//! Shared session-link metadata helpers.

// Rust guideline compliant 2026-06-26

use std::collections::BTreeMap;

use crate::{pick, Error, Provider, GITHUB_BRANCH_FIELD, LINEAR_BRANCH_FIELD};

/// Session metadata key for the link provider.
pub const LINK_PROVIDER_KEY: &str = "link.provider";
/// Session metadata key for the link kind.
pub const LINK_KIND_KEY: &str = "link.kind";
/// Session metadata key for the provider item identifier.
pub const LINK_ID_KEY: &str = "link.id";
/// Session metadata key for the provider item URL.
pub const LINK_URL_KEY: &str = "link.url";
/// Session metadata key for the launch branch.
pub const LINK_BRANCH_KEY: &str = "link.branch";

const LINEAR_PROVIDER_VALUE: &str = "linear";
const GITHUB_PROVIDER_VALUE: &str = "github";
const ISSUE_KIND_VALUE: &str = "issue";
const PULL_REQUEST_KIND_VALUE: &str = "pull_request";

/// Provider that owns session link metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkProvider {
    /// Linear issue provider.
    Linear,
    /// GitHub provider.
    GitHub,
}

impl SessionLinkProvider {
    /// Returns the stable metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => LINEAR_PROVIDER_VALUE,
            Self::GitHub => GITHUB_PROVIDER_VALUE,
        }
    }

    /// Parses the stable metadata value.
    #[must_use]
    pub const fn from_metadata(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"linear" => Some(Self::Linear),
            b"github" => Some(Self::GitHub),
            _ => None,
        }
    }
}

/// Provider item kind stored in session link metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkKind {
    /// Issue work item.
    Issue,
    /// Pull request work item.
    PullRequest,
}

impl SessionLinkKind {
    /// Returns the stable metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => ISSUE_KIND_VALUE,
            Self::PullRequest => PULL_REQUEST_KIND_VALUE,
        }
    }

    /// Parses the stable metadata value.
    #[must_use]
    pub const fn from_metadata(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"issue" => Some(Self::Issue),
            b"pull_request" => Some(Self::PullRequest),
            _ => None,
        }
    }
}

/// Opaque provider link metadata written at `session.new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLinkMetadata {
    /// Provider that owns the linked item.
    pub provider: SessionLinkProvider,
    /// Provider item kind.
    pub kind: SessionLinkKind,
    /// Provider item identifier.
    pub id: String,
    /// Provider item URL.
    pub url: String,
    /// Branch used for the launched session.
    pub branch: String,
}

impl SessionLinkMetadata {
    /// Creates validated link metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingLinkField`] when a link value is empty or
    /// whitespace-only. Returns [`Error::InvalidLinkField`] when a link value
    /// contains an ASCII control character.
    pub fn new(
        provider: SessionLinkProvider,
        kind: SessionLinkKind,
        id: impl Into<String>,
        url: impl Into<String>,
        branch: impl Into<String>,
    ) -> Result<Self, Error> {
        let link = Self {
            provider,
            kind,
            id: id.into(),
            url: url.into(),
            branch: branch.into(),
        };
        link.validate()?;
        Ok(link)
    }

    /// Returns metadata keys accepted by `session.new`.
    #[must_use]
    pub fn to_session_metadata(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                LINK_PROVIDER_KEY.to_owned(),
                self.provider.as_str().to_owned(),
            ),
            (LINK_KIND_KEY.to_owned(), self.kind.as_str().to_owned()),
            (LINK_ID_KEY.to_owned(), self.id.clone()),
            (LINK_URL_KEY.to_owned(), self.url.clone()),
            (LINK_BRANCH_KEY.to_owned(), self.branch.clone()),
        ])
    }

    fn validate(&self) -> Result<(), Error> {
        validate_link_value(LINK_ID_KEY, &self.id)?;
        validate_link_value(LINK_URL_KEY, &self.url)?;
        validate_link_value(LINK_BRANCH_KEY, &self.branch)?;
        Ok(())
    }
}

/// Returns a validated session-link metadata value.
///
/// # Errors
///
/// Returns [`Error::MissingLinkField`] when `value` is empty or whitespace-only.
/// Returns [`Error::InvalidLinkField`] when `value` contains an ASCII control
/// character.
pub fn checked_link_value(field: &'static str, value: impl Into<String>) -> Result<String, Error> {
    let value = value.into();
    validate_link_value(field, &value)?;
    Ok(value)
}

/// Extracts a provider branch from raw provider JSON.
///
/// The field precedence matches the shared prompt renderer for the selected
/// provider.
///
/// # Errors
///
/// Returns [`Error::InvalidJson`] when `raw_json` is invalid. Returns
/// [`Error::MissingRequiredField`] when the provider JSON contains no branch
/// field accepted by the selected provider.
pub fn branch_from_provider_json(provider: Provider, raw_json: &str) -> Result<String, Error> {
    let data: serde_json::Value = serde_json::from_str(raw_json).map_err(Error::InvalidJson)?;
    let field = match provider {
        Provider::GitHubPr => GITHUB_BRANCH_FIELD,
        Provider::LinearIssue => LINEAR_BRANCH_FIELD,
    };
    pick(&data, field)
}

fn validate_link_value(field: &'static str, value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(Error::MissingLinkField { field });
    }
    if value.chars().any(|ch| ch.is_ascii_control()) {
        return Err(Error::InvalidLinkField { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        branch_from_provider_json, SessionLinkKind, SessionLinkMetadata, SessionLinkProvider,
    };
    use crate::{Error, Provider};

    #[test]
    fn provider_and_kind_values_are_canonical() {
        assert_eq!(SessionLinkProvider::Linear.as_str(), "linear");
        assert_eq!(SessionLinkProvider::GitHub.as_str(), "github");
        assert_eq!(SessionLinkKind::Issue.as_str(), "issue");
        assert_eq!(SessionLinkKind::PullRequest.as_str(), "pull_request");
    }

    #[test]
    fn session_metadata_emits_the_canonical_key_set_in_order() {
        let link = SessionLinkMetadata::new(
            SessionLinkProvider::GitHub,
            SessionLinkKind::PullRequest,
            "42",
            "https://github.test/pull/42",
            "feature/link-metadata",
        )
        .expect("valid link metadata");

        let metadata = link.to_session_metadata();
        let keys: Vec<&str> = metadata.keys().map(String::as_str).collect();

        assert_eq!(
            keys,
            vec![
                "link.branch",
                "link.id",
                "link.kind",
                "link.provider",
                "link.url"
            ]
        );
        assert_eq!(
            metadata,
            BTreeMap::from([
                ("link.branch".to_owned(), "feature/link-metadata".to_owned()),
                ("link.id".to_owned(), "42".to_owned()),
                ("link.kind".to_owned(), "pull_request".to_owned()),
                ("link.provider".to_owned(), "github".to_owned()),
                (
                    "link.url".to_owned(),
                    "https://github.test/pull/42".to_owned()
                ),
            ])
        );
    }

    #[test]
    fn link_metadata_rejects_empty_and_whitespace_values() {
        for value in ["", " \t\n"] {
            let err = SessionLinkMetadata::new(
                SessionLinkProvider::Linear,
                SessionLinkKind::Issue,
                "LIN-1",
                "https://linear.test/LIN-1",
                value,
            )
            .expect_err("missing branch rejected");

            assert!(matches!(
                err,
                Error::MissingLinkField {
                    field: "link.branch"
                }
            ));
        }
    }

    #[test]
    fn link_metadata_rejects_ascii_control_characters() {
        for value in [
            "feature/line\nbreak",
            "feature/with\ttab",
            "feature/\u{7f}delete",
        ] {
            let err = SessionLinkMetadata::new(
                SessionLinkProvider::Linear,
                SessionLinkKind::Issue,
                "LIN-1",
                "https://linear.test/LIN-1",
                value,
            )
            .expect_err("control character rejected");

            assert!(matches!(
                err,
                Error::InvalidLinkField {
                    field: "link.branch"
                }
            ));
        }
    }

    #[test]
    fn link_metadata_accepts_normal_values() {
        let link = SessionLinkMetadata::new(
            SessionLinkProvider::Linear,
            SessionLinkKind::Issue,
            "LIN-1",
            "https://linear.test/LIN-1",
            "feature/link-metadata",
        )
        .expect("valid link metadata");

        assert_eq!(link.branch, "feature/link-metadata");
    }

    #[test]
    fn github_branch_selection_prefers_head_ref_name() {
        let branch = branch_from_provider_json(
            Provider::GitHubPr,
            r#"{"headRefName":"head","branch":"branch","branchName":"branch-name"}"#,
        )
        .expect("github branch");

        assert_eq!(branch, "head");
    }

    #[test]
    fn github_branch_selection_falls_back_through_branch_fields() {
        let branch = branch_from_provider_json(
            Provider::GitHubPr,
            r#"{"branch":"branch","branchName":"branch-name"}"#,
        )
        .expect("github branch field fallback");
        let branch_name =
            branch_from_provider_json(Provider::GitHubPr, r#"{"branchName":"branch-name"}"#)
                .expect("github branchName fallback");

        assert_eq!(branch, "branch");
        assert_eq!(branch_name, "branch-name");
    }

    #[test]
    fn linear_branch_selection_prefers_branch_name() {
        let branch = branch_from_provider_json(
            Provider::LinearIssue,
            r#"{"branchName":"branch-name","branch":"branch"}"#,
        )
        .expect("linear branch");

        assert_eq!(branch, "branch-name");
    }
}
