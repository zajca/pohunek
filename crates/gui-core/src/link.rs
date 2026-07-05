//! Session-link metadata, provider launch flows, and prompt-preview rendering.

use std::collections::BTreeMap;

use protocol::{ProjectActionResult, ProviderKind, SessionInfo};
use serde_json::Value;

use crate::providers;
use crate::{render_prompt, CoreError, PromptProvider};

/// Provider context used to render a prompt preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptContext {
    pub provider: PromptProvider,
    pub item_id: String,
    pub json: String,
}

/// Rendered prompt preview ready for launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPreview {
    pub prompt_name: String,
    pub rendered: String,
    pub branch: Option<String>,
}

/// Launch request for a rendered project action prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLaunchParams {
    pub project: String,
    pub action: ProjectActionResult,
    pub preview: PromptPreview,
    pub cols: u16,
    pub rows: u16,
    pub metadata: BTreeMap<String, String>,
    /// Owner-set display name for the launched session, or `None` for id-only.
    pub name: Option<String>,
}

/// Provider session link owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkProvider {
    /// Linear issue provider.
    Linear,
    /// GitHub provider.
    GitHub,
}

impl SessionLinkProvider {
    /// Stable metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::GitHub => "github",
        }
    }

    const fn from_metadata(value: &str) -> Option<Self> {
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
    /// Stable metadata value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull_request",
        }
    }

    const fn from_metadata(value: &str) -> Option<Self> {
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
    pub provider: SessionLinkProvider,
    pub kind: SessionLinkKind,
    pub id: String,
    pub url: String,
    pub branch: String,
}

impl SessionLinkMetadata {
    /// Creates validated link metadata.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when an opaque link value is empty.
    pub fn new(
        provider: SessionLinkProvider,
        kind: SessionLinkKind,
        id: impl Into<String>,
        url: impl Into<String>,
        branch: impl Into<String>,
    ) -> Result<Self, CoreError> {
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
                "link.provider".to_owned(),
                self.provider.as_str().to_owned(),
            ),
            ("link.kind".to_owned(), self.kind.as_str().to_owned()),
            ("link.id".to_owned(), self.id.clone()),
            ("link.url".to_owned(), self.url.clone()),
            ("link.branch".to_owned(), self.branch.clone()),
        ])
    }

    fn validate(&self) -> Result<(), CoreError> {
        checked_link_value("link.id", self.id.clone())?;
        checked_link_value("link.url", self.url.clone())?;
        checked_link_value("link.branch", self.branch.clone())?;
        Ok(())
    }
}

fn checked_link_value(field: &'static str, value: String) -> Result<String, CoreError> {
    if value.trim().is_empty() {
        Err(CoreError::MissingLinkField { field })
    } else {
        Ok(value)
    }
}

/// Provider item context used to resolve, render, launch, and link a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchItem {
    pub(crate) action_provider: ProviderKind,
    pub(crate) prompt_provider: PromptProvider,
    pub(crate) item_id: String,
    pub(crate) context_json: String,
    link_provider: SessionLinkProvider,
    link_kind: SessionLinkKind,
    link_url: String,
}

/// Provider launch request that resolves a project action before `session.new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchParams {
    pub project: String,
    pub action_name: String,
    pub item: ProviderLaunchItem,
    pub cols: u16,
    pub rows: u16,
    /// Owner-set display name for the launched session, or `None` for id-only.
    pub name: Option<String>,
}

impl ProviderLaunchItem {
    /// Builds a launch context for a Linear issue.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when required link metadata is empty.
    pub fn linear_issue(
        item_id: impl Into<String>,
        context_json: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let item_id = checked_link_value("link.id", item_id.into())?;
        Ok(Self {
            action_provider: ProviderKind::LinearIssue,
            prompt_provider: PromptProvider::LinearIssue,
            link_provider: SessionLinkProvider::Linear,
            link_kind: SessionLinkKind::Issue,
            link_url: checked_link_value("link.url", url.into())?,
            item_id,
            context_json: context_json.into(),
        })
    }

    /// Builds a launch context for a GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when required link metadata is empty.
    pub fn github_pull_request(
        number: impl Into<String>,
        context_json: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let number = checked_link_value("link.id", number.into())?;
        Ok(Self {
            action_provider: ProviderKind::GithubPr,
            prompt_provider: PromptProvider::GitHubPr,
            link_provider: SessionLinkProvider::GitHub,
            link_kind: SessionLinkKind::PullRequest,
            link_url: checked_link_value("link.url", url.into())?,
            item_id: number,
            context_json: context_json.into(),
        })
    }

    pub(crate) fn validate_link_invariants(&self) -> Result<(), CoreError> {
        let expected = match (
            &self.action_provider,
            self.prompt_provider,
            self.link_provider,
            self.link_kind,
        ) {
            (
                ProviderKind::LinearIssue,
                PromptProvider::LinearIssue,
                SessionLinkProvider::Linear,
                SessionLinkKind::Issue,
            )
            | (
                ProviderKind::GithubPr,
                PromptProvider::GitHubPr,
                SessionLinkProvider::GitHub,
                SessionLinkKind::PullRequest,
            ) => return Ok(()),
            _ => "action provider, prompt provider, and link metadata must describe the same provider item",
        };
        Err(CoreError::ProviderLaunchItemMismatch { message: expected })
    }

    pub(crate) fn to_session_link(
        &self,
        branch: impl Into<String>,
    ) -> Result<SessionLinkMetadata, CoreError> {
        SessionLinkMetadata::new(
            self.link_provider,
            self.link_kind,
            self.item_id.clone(),
            self.link_url.clone(),
            branch,
        )
    }
}

/// One stable metadata row for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRow {
    pub key: String,
    pub value: String,
}

/// Return metadata rows in wire-stable key order.
#[must_use]
pub fn session_metadata_rows(session: &SessionInfo) -> Vec<MetadataRow> {
    session
        .metadata
        .iter()
        .map(|(key, value)| MetadataRow {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// Return parsed provider link metadata when a session is linked.
#[must_use]
pub fn session_link_metadata(session: &SessionInfo) -> Option<SessionLinkMetadata> {
    let provider =
        SessionLinkProvider::from_metadata(session.metadata.get("link.provider")?.as_str())?;
    let kind = SessionLinkKind::from_metadata(session.metadata.get("link.kind")?.as_str())?;
    let id = session.metadata.get("link.id")?.clone();
    let url = session.metadata.get("link.url")?.clone();
    let branch = session.metadata.get("link.branch")?.clone();
    SessionLinkMetadata::new(provider, kind, id, url, branch).ok()
}

/// Render a resolved prompt template for preview.
pub fn preview_prompt_content(
    prompt_name: impl Into<String>,
    template_content: impl AsRef<str>,
    context: &PromptContext,
) -> Result<PromptPreview, CoreError> {
    let rendered = render_prompt(
        template_content.as_ref(),
        context.provider,
        context.item_id.as_str(),
        context.json.as_str(),
    )?;
    let branch = branch_from_context(context.provider, context.json.as_str())?;
    Ok(PromptPreview {
        prompt_name: prompt_name.into(),
        rendered,
        branch: Some(branch),
    })
}

/// Render a resolved project action prompt for preview.
pub fn preview_action_prompt(
    action: &ProjectActionResult,
    item_id: impl Into<String>,
    context_json: impl Into<String>,
) -> Result<PromptPreview, CoreError> {
    match &action.provider {
        ProviderKind::LinearIssue | ProviderKind::GithubPr => {
            let prompt_provider = action_prompt_provider(&action.provider)?;
            preview_prompt_content(
                action.prompt_name.clone(),
                &action.prompt_content,
                &PromptContext {
                    provider: prompt_provider,
                    item_id: item_id.into(),
                    json: context_json.into(),
                },
            )
        }
        ProviderKind::None => {
            let rendered = pohunek_prompt::render_static(&action.prompt_content)?;
            Ok(PromptPreview {
                prompt_name: action.prompt_name.clone(),
                rendered,
                branch: action.branch.clone(),
            })
        }
    }
}

pub(crate) fn action_prompt_provider(provider: &ProviderKind) -> Result<PromptProvider, CoreError> {
    match provider {
        ProviderKind::LinearIssue => Ok(PromptProvider::LinearIssue),
        ProviderKind::GithubPr => Ok(PromptProvider::GitHubPr),
        ProviderKind::None => Err(CoreError::UnsupportedPromptProvider {
            provider: provider.as_str(),
        }),
    }
}

fn branch_from_context(provider: PromptProvider, raw_json: &str) -> Result<String, CoreError> {
    let data: Value = serde_json::from_str(raw_json)?;
    let fields = match provider {
        PromptProvider::LinearIssue => providers::linear::ISSUE_BRANCH_FIELDS,
        PromptProvider::GitHubPr => providers::github::PULL_REQUEST_BRANCH_FIELDS,
    };
    fields
        .iter()
        .find_map(|field| {
            data.get(*field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .ok_or(CoreError::MissingPromptBranch {
            provider: provider.as_str(),
        })
}
