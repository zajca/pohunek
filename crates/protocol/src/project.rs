//! Typed payloads for the `project.*` method family.
//!
//! A **project** is a git repository the daemon has seen on its host, keyed
//! internally by the canonical `git_common_dir` (see `docs/design/projects.md`).
//! These shared types define the JSON shapes the CLI and daemon exchange inside
//! the generic request/response envelopes for project lifecycle methods, mirroring
//! the role [`crate::SessionInfo`] plays for sessions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How a project entered the store.
///
/// `Auto` records accrue as a side effect of starting sessions inside a work
/// tree; `Manual` records are added explicitly via `pohunek project add`.
/// Re-adding an `Auto` project flips it to `Manual` so it is never treated as
/// stale auto data (if garbage collection is ever added).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectSource.ts"))]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    /// Auto-registered when a session started inside the repository's work tree.
    Auto,
    /// Added (or re-added) explicitly by the operator.
    Manual,
}

impl ProjectSource {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

/// Summary of one project, the wire/list shape returned by `project.*` methods
/// (mirrors [`crate::SessionInfo`]'s role for sessions). The `id` and `label` are
/// the daemon-derived display handles; the canonical `git_common_dir` is the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectInfo.ts"))]
pub struct ProjectInfo {
    /// Stable derived id (`p-…`, an FNV-1a hash of the canonical key).
    pub id: String,
    /// Display label: the custom name, else the repo-root basename.
    pub label: String,
    /// The repository's main checkout (where an in-place session runs).
    pub repo_root: PathBuf,
    /// The git common dir — the project's identity key (canonical, absolute).
    pub git_common_dir: PathBuf,
    /// The `origin` remote URL, credentials redacted; absent when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub origin_url: Option<String>,
    /// Base branch new worktrees branch from; absent ⇒ repo HEAD at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default_base_branch: Option<String>,
    /// Whether the record was auto-registered or added explicitly.
    pub source: ProjectSource,
    /// Whether the repository is bare (no working tree to run in-place).
    pub is_bare: bool,
    /// Registration timestamp (RFC3339).
    pub added_at: String,
    /// Last-used timestamp (RFC3339).
    pub last_used_at: String,
}

/// Parameters for `project.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectListParams.ts"))]
pub struct ProjectListParams {
    /// Exact-match filters applied with AND semantics (mirrors `session.list`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<ProjectListFilter>,
}

/// A single exact-match `project list --filter key=value` predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectListFilter.ts"))]
#[serde(tag = "key", content = "value", rename_all = "snake_case")]
pub enum ProjectListFilter {
    /// Match [`ProjectInfo::source`] (`auto`/`manual`).
    Source(ProjectSource),
    /// Match [`ProjectInfo::label`].
    Label(String),
    /// Match [`ProjectInfo::id`].
    Id(String),
}

impl ProjectListFilter {
    /// Whether this filter matches `project` exactly.
    #[must_use]
    pub fn matches(&self, project: &ProjectInfo) -> bool {
        match self {
            Self::Source(source) => project.source == *source,
            Self::Label(label) => project.label == *label,
            Self::Id(id) => project.id == *id,
        }
    }
}

/// Parameters for `project.add`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectAddParams.ts"))]
pub struct ProjectAddParams {
    /// Path to register, on the **target host**. `None` means the caller's `cwd`
    /// (local only); a remote add must give a path valid on that host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub path: Option<PathBuf>,
    /// Optional custom display name to set on the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    /// Optional default base branch for worktrees created against the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub base_branch: Option<String>,
}

/// Parameters for `project.show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectShowParams.ts"))]
pub struct ProjectShowParams {
    /// The `<id|label>` reference to resolve against the host's store.
    pub reference: String,
}

/// Parameters for `project.rename`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectRenameParams.ts"))]
pub struct ProjectRenameParams {
    /// The `<id|label>` reference to resolve.
    pub reference: String,
    /// The new custom display name.
    pub name: String,
}

/// Parameters for `project.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectRemoveParams.ts"))]
pub struct ProjectRemoveParams {
    /// The `<id|label>` reference to resolve.
    pub reference: String,
    /// Also remove pohunek-owned worktrees for the project (never the main
    /// checkout or unowned worktrees). Honored from the worktree-linkage milestone.
    #[serde(default)]
    pub prune_worktrees: bool,
}

/// One worktree of a project as seen live via `git worktree list --porcelain`,
/// enriched with pohunek's own view (whether it created the worktree, and whether
/// a live session runs in it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectWorktree.ts"))]
pub struct ProjectWorktree {
    /// Absolute path of the worktree.
    pub path: PathBuf,
    /// Checked-out branch; absent on a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub branch: Option<String>,
    /// Current HEAD commit, when git reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub head: Option<String>,
    /// Whether git reports this entry as the bare main repository.
    pub bare: bool,
    /// Whether git reports the worktree as locked.
    pub locked: bool,
    /// Whether pohunek created this worktree (a binding records it for the project).
    pub owned: bool,
    /// The id of a live pohunek session running in this worktree, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<String>,
}

/// Result of `project.show`: the project plus its live worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectShowResult.ts"))]
pub struct ProjectShowResult {
    /// The project record (display shape).
    pub project: ProjectInfo,
    /// Worktrees reported live by git, enriched with ownership/session info.
    pub worktrees: Vec<ProjectWorktree>,
}

/// Which config layer a `project.prompt`/`project.action` definition resolved from.
///
/// In-repo `<repo_root>/.pohunek/` shadows the host-default `<config_dir>/` layer
/// per name; this records which one won so a caller (and `--json`) can see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PromptLayer.ts"))]
#[serde(rename_all = "snake_case")]
pub enum PromptLayer {
    /// Resolved from the repository's in-repo `.pohunek/` (travels with the repo).
    InRepo,
    /// Resolved from the host-default `~/.config/pohunek/` on the daemon's host.
    Host,
}

/// Parameters for `project.prompt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectPromptParams.ts"))]
pub struct ProjectPromptParams {
    /// The `<id|label>` reference to resolve against the host's store.
    pub reference: String,
    /// The prompt name to resolve. A single path segment (the daemon enforces the
    /// `^[A-Za-z0-9._-]+$` name guard before any read); required — there is no
    /// "default prompt".
    pub name: String,
}

/// Result of `project.prompt`: the resolved template content (fail-closed) and the
/// layer it came from. The daemon does **not** render it — provider data and
/// rendering stay caller-side (A.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectPromptResult.ts"))]
pub struct ProjectPromptResult {
    /// The resolved prompt name (echoed back).
    pub name: String,
    /// The raw prompt-template content, with `${var}` placeholders intact.
    pub content: String,
    /// Which layer the prompt was resolved from.
    pub layer: PromptLayer,
}

/// Provider-specific reference type an action expects the launcher to supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProviderKind.ts"))]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// A Linear issue reference; the launcher fetches Linear data caller-side.
    LinearIssue,
    /// A GitHub pull request reference; the launcher fetches GitHub data caller-side.
    GithubPr,
    /// No provider-derived branch/data; static action metadata is enough.
    None,
}

impl ProviderKind {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::LinearIssue => "linear_issue",
            Self::GithubPr => "github_pr",
            Self::None => "none",
        }
    }
}

/// Parameters for `project.action`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectActionParams.ts"))]
pub struct ProjectActionParams {
    /// The `<id|label>` reference to resolve against the host's store.
    pub reference: String,
    /// The action name to resolve. A single path segment enforced daemon-side.
    pub name: String,
}

/// Result of `project.action`: a resolved launch recipe plus prompt content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectActionResult.ts"))]
pub struct ProjectActionResult {
    /// Which provider data shape the caller should fetch/render for this action.
    pub provider: ProviderKind,
    /// Agent name to pass through verbatim to `session new --agent`.
    pub agent: String,
    /// Template-selected base branch; absent means fall through to project default / HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub base_branch: Option<String>,
    /// Static branch for provider-less actions; absent when provider supplies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub branch: Option<String>,
    /// Name of the prompt template used to populate [`Self::prompt_content`].
    pub prompt_name: String,
    /// Raw prompt-template content, with `${var}` placeholders intact.
    pub prompt_content: String,
}

/// Parameters for `project.actions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectActionsParams.ts"))]
pub struct ProjectActionsParams {
    /// The `<id|label>` reference to resolve against the host's store.
    pub reference: String,
}

/// Result of `project.actions`: available action summaries after layer shadowing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectActionsResult.ts"))]
pub struct ProjectActionsResult {
    /// Available action definitions.
    pub actions: Vec<ActionSummary>,
}

/// One available action after applying in-repo-over-host shadowing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ActionSummary.ts"))]
pub struct ActionSummary {
    /// Action name.
    pub name: String,
    /// Provider required by this action.
    pub provider: ProviderKind,
    /// Template name this action references.
    pub template: String,
    /// Winning layer for this action definition.
    pub layer: PromptLayer,
}

/// Result of `project.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProjectRemoveResult.ts"))]
pub struct ProjectRemoveResult {
    /// Whether a project record was removed.
    pub removed: bool,
    /// Number of pohunek-owned worktrees pruned (`0` unless `prune_worktrees`).
    pub pruned_worktrees: usize,
    /// Ids of live sessions whose worktrees were **skipped** by the prune (a
    /// running session was using the worktree, so it was left in place). Empty
    /// unless `prune_worktrees` and a session was live in an owned worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_worktrees: Vec<String>,
}

/// Parameters for `worktree.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "WorktreeRemoveParams.ts"))]
pub struct WorktreeRemoveParams {
    /// Absolute path of the worktree to remove, as reported by `project.show`.
    /// The daemon matches it against its worktree bindings (canonicalized), so
    /// only a pohunek-owned worktree resolves; an external worktree fails closed.
    pub path: PathBuf,
}

/// Result of `worktree.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "WorktreeRemoveResult.ts"))]
pub struct WorktreeRemoveResult {
    /// Whether the worktree (and its binding) was removed. Always `true` on a
    /// successful response; the failure cases (`worktree_not_owned`,
    /// `worktree_in_use`) surface as typed errors instead.
    pub removed: bool,
}

#[cfg(test)]
mod tests {
    use super::{ProjectSource, ProviderKind};

    #[test]
    fn project_source_strings_match_wire_repr() {
        assert_eq!(ProjectSource::Auto.as_str(), "auto");
        assert_eq!(ProjectSource::Manual.as_str(), "manual");
    }

    #[test]
    fn provider_kind_strings_match_wire_repr() {
        assert_eq!(ProviderKind::LinearIssue.as_str(), "linear_issue");
        assert_eq!(ProviderKind::GithubPr.as_str(), "github_pr");
        assert_eq!(ProviderKind::None.as_str(), "none");
    }
}
