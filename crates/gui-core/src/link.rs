//! Session-link metadata, provider launch flows, and prompt-preview rendering.

use std::collections::BTreeMap;

use pohunek_prompt::link::{
    branch_from_provider_json, checked_link_value, LINK_BRANCH_KEY, LINK_ID_KEY, LINK_KIND_KEY,
    LINK_PROVIDER_KEY, LINK_URL_KEY,
};
pub use pohunek_prompt::link::{SessionLinkKind, SessionLinkMetadata, SessionLinkProvider};
use protocol::{ProjectActionResult, ProviderKind, SessionInfo};

use crate::{render_prompt, CoreError, PromptError, PromptProvider};

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
    /// Returns [`CoreError::MissingLinkField`] when required link metadata is
    /// empty. Returns [`CoreError::InvalidLinkField`] when required link
    /// metadata contains an ASCII control character.
    pub fn linear_issue(
        item_id: impl Into<String>,
        context_json: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let item_id = checked_link_value(LINK_ID_KEY, item_id).map_err(link_error_to_core)?;
        Ok(Self {
            action_provider: ProviderKind::LinearIssue,
            prompt_provider: PromptProvider::LinearIssue,
            link_provider: SessionLinkProvider::Linear,
            link_kind: SessionLinkKind::Issue,
            link_url: checked_link_value(LINK_URL_KEY, url).map_err(link_error_to_core)?,
            item_id,
            context_json: context_json.into(),
        })
    }

    /// Builds a launch context for a GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingLinkField`] when required link metadata is
    /// empty. Returns [`CoreError::InvalidLinkField`] when required link
    /// metadata contains an ASCII control character.
    pub fn github_pull_request(
        number: impl Into<String>,
        context_json: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let number = checked_link_value(LINK_ID_KEY, number).map_err(link_error_to_core)?;
        Ok(Self {
            action_provider: ProviderKind::GithubPr,
            prompt_provider: PromptProvider::GitHubPr,
            link_provider: SessionLinkProvider::GitHub,
            link_kind: SessionLinkKind::PullRequest,
            link_url: checked_link_value(LINK_URL_KEY, url).map_err(link_error_to_core)?,
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
        .map_err(link_error_to_core)
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
        SessionLinkProvider::from_metadata(session.metadata.get(LINK_PROVIDER_KEY)?.as_str())?;
    let kind = SessionLinkKind::from_metadata(session.metadata.get(LINK_KIND_KEY)?.as_str())?;
    let id = session.metadata.get(LINK_ID_KEY)?.clone();
    let url = session.metadata.get(LINK_URL_KEY)?.clone();
    let branch = session.metadata.get(LINK_BRANCH_KEY)?.clone();
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
    branch_from_provider_json(provider, raw_json).map_err(|err| branch_error_to_core(provider, err))
}

fn branch_error_to_core(provider: PromptProvider, err: PromptError) -> CoreError {
    match err {
        PromptError::InvalidJson(err) => CoreError::Json(err),
        PromptError::MissingRequiredField(_) => CoreError::MissingPromptBranch {
            provider: provider.as_str(),
        },
        err => CoreError::Prompt(err),
    }
}

fn link_error_to_core(err: PromptError) -> CoreError {
    match err {
        PromptError::MissingLinkField { field } => CoreError::MissingLinkField { field },
        PromptError::InvalidLinkField { field } => CoreError::InvalidLinkField { field },
        err => CoreError::Prompt(err),
    }
}
