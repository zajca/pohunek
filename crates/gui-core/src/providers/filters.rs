//! Named provider filters for the GitHub and Linear browser panels.
//!
//! A filter is a saved view the operator can pick from in the GUI: a GitHub
//! pull request filter carries a raw `gh` search query, a Linear filter carries
//! a raw Linear `IssueFilter`. Filters are resolved from two client-side layers
//! (host `gui.toml` and the project's in-repo `.pohunek/providers.toml`) plus a
//! built-in fallback; the daemon never sees them, matching the provider trust
//! boundary in `AGENTS.md`.

// Rust guideline compliant 2026-06-30

use serde_json::{json, Value};

/// Pull request state a GitHub filter restricts to, mapped to `gh --state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitHubPrState {
    /// Open pull requests (`gh --state open`); the `gh pr list` default.
    #[default]
    Open,
    /// Closed-but-unmerged pull requests (`gh --state closed`).
    Closed,
    /// Merged pull requests (`gh --state merged`).
    Merged,
    /// Every pull request regardless of state (`gh --state all`).
    All,
}

impl GitHubPrState {
    /// Returns the value passed to `gh pr list --state`.
    #[must_use]
    pub const fn as_gh_arg(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }

    /// Parses a `gh --state` value, accepting only the supported variants.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError::UnknownGitHubState`] for any other value.
    pub fn parse(value: &str) -> Result<Self, FilterError> {
        match value {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "merged" => Ok(Self::Merged),
            "all" => Ok(Self::All),
            other => Err(FilterError::UnknownGitHubState {
                value: other.to_owned(),
            }),
        }
    }
}

/// A named GitHub pull request filter expressed as a raw `gh` search query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubFilter {
    /// Display name shown in the GUI picker; the filter's identity for shadowing.
    pub name: String,
    /// `gh pr list --search` query, such as `author:@me` or `review:approved`.
    /// Empty means no search qualifier (state alone selects the view).
    pub search: String,
    /// Pull request state restriction passed to `gh pr list --state`.
    pub state: GitHubPrState,
}

impl GitHubFilter {
    /// Creates a GitHub pull request filter.
    #[must_use]
    pub fn new(name: impl Into<String>, search: impl Into<String>, state: GitHubPrState) -> Self {
        Self {
            name: name.into(),
            search: search.into(),
            state,
        }
    }

    /// Returns the extra `gh pr list` arguments selecting this filter's view.
    ///
    /// The returned arguments are appended to the base `pr list --json …`
    /// command; `--search` is omitted when the query is blank so an empty
    /// filter keeps the plain state-only listing.
    #[must_use]
    pub fn gh_args(&self) -> Vec<String> {
        let mut args = vec!["--state".to_owned(), self.state.as_gh_arg().to_owned()];
        let search = self.search.trim();
        if !search.is_empty() {
            args.push("--search".to_owned());
            args.push(search.to_owned());
        }
        args
    }
}

/// A named Linear filter expressed as a raw Linear `IssueFilter` JSON value.
#[derive(Debug, Clone, PartialEq)]
pub struct LinearFilter {
    /// Display name shown in the GUI picker; the filter's identity for shadowing.
    pub name: String,
    /// Raw Linear `IssueFilter` object passed verbatim as the `$filter` variable
    /// to the top-level `issues(filter:)` query.
    pub filter: Value,
}

impl LinearFilter {
    /// Creates a Linear filter from a raw `IssueFilter` value.
    #[must_use]
    pub fn new(name: impl Into<String>, filter: Value) -> Self {
        Self {
            name: name.into(),
            filter,
        }
    }
}

/// A resolved set of provider filters for both panels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderFilterSet {
    /// GitHub pull request filters, in display order.
    pub github: Vec<GitHubFilter>,
    /// Linear issue filters, in display order.
    pub linear: Vec<LinearFilter>,
}

impl ProviderFilterSet {
    /// Built-in defaults used when a provider has no configured filters.
    ///
    /// GitHub queries use `gh` search syntax; Linear values are raw
    /// `IssueFilter` objects. The Linear defaults restrict by workflow-state
    /// `type` (workspace-agnostic) rather than state name, which varies per
    /// workspace; operators add name-based filters via configuration.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            github: vec![
                GitHubFilter::new("All open", "", GitHubPrState::Open),
                GitHubFilter::new("My PRs", "author:@me", GitHubPrState::Open),
                GitHubFilter::new("Assigned to me", "assignee:@me", GitHubPrState::Open),
                GitHubFilter::new(
                    "Review requested",
                    "review-requested:@me",
                    GitHubPrState::Open,
                ),
                GitHubFilter::new("Ready to merge", "review:approved", GitHubPrState::Open),
            ],
            linear: vec![
                LinearFilter::new(
                    "Assigned to me",
                    json!({ "assignee": { "isMe": { "eq": true } } }),
                ),
                LinearFilter::new(
                    "Created by me",
                    json!({ "creator": { "isMe": { "eq": true } } }),
                ),
                LinearFilter::new(
                    "Active",
                    json!({ "state": { "type": { "in": ["started"] } } }),
                ),
                LinearFilter::new(
                    "Backlog",
                    json!({ "state": { "type": { "in": ["backlog", "triage"] } } }),
                ),
            ],
        }
    }

    /// Returns the filter names in display order, for a picker.
    #[must_use]
    pub fn github_names(&self) -> Vec<String> {
        self.github
            .iter()
            .map(|filter| filter.name.clone())
            .collect()
    }

    /// Returns the filter names in display order, for a picker.
    #[must_use]
    pub fn linear_names(&self) -> Vec<String> {
        self.linear
            .iter()
            .map(|filter| filter.name.clone())
            .collect()
    }

    /// Finds a GitHub filter by name.
    #[must_use]
    pub fn github_filter(&self, name: &str) -> Option<&GitHubFilter> {
        self.github.iter().find(|filter| filter.name == name)
    }

    /// Finds a Linear filter by name.
    #[must_use]
    pub fn linear_filter(&self, name: &str) -> Option<&LinearFilter> {
        self.linear.iter().find(|filter| filter.name == name)
    }
}

/// Resolves the effective filters from the host and optional project layers.
///
/// The project (in-repo `.pohunek/providers.toml`) layer shadows the host
/// (`gui.toml`) layer per filter name: a project filter replaces a host filter
/// with the same name in place, and project-only filters append after the host
/// ones. A provider left with no filters after merging falls back to
/// [`ProviderFilterSet::builtin`] for that provider, so a host that configures
/// only one provider still gets defaults for the other.
#[must_use]
pub fn merge(host: &ProviderFilterSet, project: Option<&ProviderFilterSet>) -> ProviderFilterSet {
    let mut merged = match project {
        Some(project) => ProviderFilterSet {
            github: shadow(&host.github, &project.github, |filter| filter.name.as_str()),
            linear: shadow(&host.linear, &project.linear, |filter| filter.name.as_str()),
        },
        None => host.clone(),
    };

    let builtin = ProviderFilterSet::builtin();
    if merged.github.is_empty() {
        merged.github = builtin.github;
    }
    if merged.linear.is_empty() {
        merged.linear = builtin.linear;
    }
    merged
}

/// Merges `overrides` over `base` by the key returned from `key`, preserving
/// `base` order for shared and base-only entries and appending override-only
/// entries last.
fn shadow<T, F>(base: &[T], overrides: &[T], key: F) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> &str,
{
    let mut merged: Vec<T> = base
        .iter()
        .map(|item| {
            overrides
                .iter()
                .find(|candidate| key(candidate) == key(item))
                .unwrap_or(item)
                .clone()
        })
        .collect();
    for item in overrides {
        if !base.iter().any(|existing| key(existing) == key(item)) {
            merged.push(item.clone());
        }
    }
    merged
}

/// Errors raised while parsing provider filter configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    /// A `gh` pull request state value was not one of the supported variants.
    #[error("unknown GitHub pull request state `{value}`; expected open, closed, merged, or all")]
    UnknownGitHubState {
        /// The unrecognized state value.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{merge, FilterError, GitHubFilter, GitHubPrState, LinearFilter, ProviderFilterSet};
    use serde_json::json;

    #[test]
    fn gh_args_omits_search_when_blank() {
        let filter = GitHubFilter::new("All open", "  ", GitHubPrState::Open);
        assert_eq!(
            filter.gh_args(),
            vec!["--state".to_owned(), "open".to_owned()]
        );
    }

    #[test]
    fn gh_args_includes_trimmed_search() {
        let filter = GitHubFilter::new("My PRs", " author:@me ", GitHubPrState::All);
        assert_eq!(
            filter.gh_args(),
            vec![
                "--state".to_owned(),
                "all".to_owned(),
                "--search".to_owned(),
                "author:@me".to_owned(),
            ]
        );
    }

    #[test]
    fn gh_state_round_trips() {
        for state in [
            GitHubPrState::Open,
            GitHubPrState::Closed,
            GitHubPrState::Merged,
            GitHubPrState::All,
        ] {
            assert_eq!(GitHubPrState::parse(state.as_gh_arg()), Ok(state));
        }
        assert_eq!(
            GitHubPrState::parse("bogus"),
            Err(FilterError::UnknownGitHubState {
                value: "bogus".to_owned()
            })
        );
    }

    #[test]
    fn builtin_has_both_providers() {
        let builtin = ProviderFilterSet::builtin();
        assert!(builtin.github_filter("My PRs").is_some());
        assert!(builtin.linear_filter("Assigned to me").is_some());
    }

    #[test]
    fn merge_without_project_returns_host() {
        let host = ProviderFilterSet {
            github: vec![GitHubFilter::new("Mine", "author:@me", GitHubPrState::Open)],
            linear: vec![LinearFilter::new("Mine", json!({ "a": 1 }))],
        };
        assert_eq!(merge(&host, None), host);
    }

    #[test]
    fn merge_falls_back_to_builtin_per_provider() {
        // Host configures only GitHub; Linear should still get built-in defaults.
        let host = ProviderFilterSet {
            github: vec![GitHubFilter::new("Mine", "author:@me", GitHubPrState::Open)],
            linear: Vec::new(),
        };
        let merged = merge(&host, None);
        assert_eq!(merged.github_names(), vec!["Mine".to_owned()]);
        assert_eq!(merged.linear, ProviderFilterSet::builtin().linear);
    }

    #[test]
    fn merge_project_shadows_host_by_name_and_appends() {
        let host = ProviderFilterSet {
            github: vec![
                GitHubFilter::new("My PRs", "author:@me", GitHubPrState::Open),
                GitHubFilter::new("All", "", GitHubPrState::All),
            ],
            linear: Vec::new(),
        };
        let project = ProviderFilterSet {
            github: vec![
                // Same name shadows the host entry in place.
                GitHubFilter::new("My PRs", "author:@me label:urgent", GitHubPrState::Open),
                // New name appends after the host entries.
                GitHubFilter::new("Stale", "draft:false", GitHubPrState::Open),
            ],
            linear: Vec::new(),
        };
        let merged = merge(&host, Some(&project));
        assert_eq!(
            merged.github_names(),
            vec!["My PRs".to_owned(), "All".to_owned(), "Stale".to_owned()]
        );
        assert_eq!(
            merged.github_filter("My PRs").map(|f| f.search.as_str()),
            Some("author:@me label:urgent")
        );
    }
}
