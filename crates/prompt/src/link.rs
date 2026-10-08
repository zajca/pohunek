//! Shared session-link metadata helpers.

// Rust guideline compliant 2026-07-19

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
    let field = match provider {
        Provider::GitHubPr => GITHUB_BRANCH_FIELD,
        Provider::LinearIssue => LINEAR_BRANCH_FIELD,
    };
    let data: serde_json::Value = serde_json::from_str(raw_json).map_err(Error::InvalidJson)?;
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
