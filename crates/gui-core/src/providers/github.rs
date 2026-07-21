//! GitHub provider client for the native GUI.
//!
//! The production client shells out to `gh` and never reads or stores GitHub
//! tokens. Authentication remains owned by the GitHub CLI installation.

// Rust guideline compliant 2026-06-26

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::process::Command;

// Prompt rendering needs `number,title,body,headRefName,url`; the remaining
// fields enrich the GUI list (author, draft flag, labels, review/CI status, diff
// size). `reviewDecision` and `statusCheckRollup` make per-PR status available
// from the single `gh pr list` call, avoiding extra per-PR requests.
const PR_FIELDS: &str = "number,title,body,headRefName,url,author,isDraft,labels,reviewDecision,statusCheckRollup,additions,deletions,updatedAt";
const ISSUE_FIELDS: &str = "number,title,body,url";
const CHECK_FIELDS: &str = "name,state,bucket,link";
const REVIEW_FIELDS: &str = "reviewDecision";
const DEFAULT_GH_TIMEOUT: Duration = Duration::from_secs(20);
const PENDING_CHECKS_EXIT_CODE: i32 = 8;
/// Primary GitHub pull request branch field emitted by `gh` and prompt JSON.
pub const PULL_REQUEST_PRIMARY_BRANCH_FIELD: &str = "headRefName";
/// Branch fields accepted for GitHub pull request prompt contexts.
pub const PULL_REQUEST_BRANCH_FIELDS: &[&str] = &[
    PULL_REQUEST_PRIMARY_BRANCH_FIELD,
    super::COMPAT_BRANCH_FIELD,
    "branchName",
];

/// Future returned by a GitHub CLI runner.
pub type GhFuture<'a> = Pin<Box<dyn Future<Output = Result<GhOutput, GitHubError>> + Send + 'a>>;

/// Configuration for GitHub CLI process execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubConfig {
    /// Path or command name for the GitHub CLI executable.
    pub gh_bin: PathBuf,
    /// Repository working directory used for GitHub CLI commands.
    pub repo_cwd: Option<PathBuf>,
    /// Maximum duration for one GitHub CLI command.
    pub timeout: Duration,
}

impl GitHubConfig {
    /// Creates GitHub CLI configuration.
    #[must_use]
    pub fn new(gh_bin: impl Into<PathBuf>) -> Self {
        Self {
            gh_bin: gh_bin.into(),
            repo_cwd: None,
            timeout: DEFAULT_GH_TIMEOUT,
        }
    }

    /// Sets the repository working directory for GitHub CLI commands.
    #[must_use]
    pub fn with_repo_cwd(mut self, repo_cwd: impl Into<PathBuf>) -> Self {
        self.repo_cwd = Some(repo_cwd.into());
        self
    }

    /// Sets the command timeout for GitHub CLI commands.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Output captured from one GitHub CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhOutput {
    /// Process exit status code, or `-1` when the platform did not provide one.
    pub status: i32,
    /// UTF-8 decoded stdout.
    pub stdout: String,
    /// UTF-8 decoded stderr.
    pub stderr: String,
}

/// Abstraction over GitHub CLI process execution.
pub trait GhRunner: Send + Sync {
    /// Runs `gh` with the supplied arguments.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] when the process cannot be started or
    /// output cannot be captured.
    fn run(&self, args: Vec<String>) -> GhFuture<'_>;
}

/// GitHub pull request fields used by prompt rendering and the GUI list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequest {
    /// Pull request number.
    pub number: u64,
    /// Pull request title.
    pub title: String,
    /// Pull request body.
    #[serde(default, deserialize_with = "empty_string_from_null")]
    pub body: String,
    /// Head branch name.
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    /// Browser URL for the pull request.
    pub url: String,
    /// Author login, when reported.
    #[serde(default, deserialize_with = "author_login")]
    pub author: Option<String>,
    /// Whether the pull request is a draft.
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
    /// Labels attached to the pull request.
    #[serde(default, deserialize_with = "labels_from_gh")]
    pub labels: Vec<GitHubLabel>,
    /// Review decision reported by GitHub.
    #[serde(rename = "reviewDecision", default)]
    pub review_decision: ReviewDecision,
    /// Check runs from the status check rollup.
    #[serde(
        rename = "statusCheckRollup",
        default,
        deserialize_with = "checks_from_rollup"
    )]
    pub checks: Vec<CheckRun>,
    /// Lines added by the pull request.
    #[serde(default)]
    pub additions: u64,
    /// Lines removed by the pull request.
    #[serde(default)]
    pub deletions: u64,
    /// Last update timestamp (RFC 3339), when reported.
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<String>,
}

impl GitHubPullRequest {
    /// Creates a pull request with empty list metadata.
    ///
    /// Display fields (author, labels, status, diff size) default to empty;
    /// production values arrive via deserialization of `gh` output.
    #[must_use]
    pub fn new(
        number: u64,
        title: impl Into<String>,
        body: impl Into<String>,
        head_ref_name: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            number,
            title: title.into(),
            body: body.into(),
            head_ref_name: head_ref_name.into(),
            url: url.into(),
            author: None,
            is_draft: false,
            labels: Vec::new(),
            review_decision: ReviewDecision::None,
            checks: Vec::new(),
            additions: 0,
            deletions: 0,
            updated_at: None,
        }
    }

    /// Returns the item id used by shared prompt rendering.
    #[must_use]
    pub fn prompt_item_id(&self) -> String {
        self.number.to_string()
    }

    /// Converts this pull request to the shared prompt renderer JSON shape.
    #[must_use]
    pub fn to_prompt_json(&self) -> Value {
        let id = self.prompt_item_id();
        let mut value = Map::new();
        value.insert("id".to_owned(), json!(id));
        value.insert("number".to_owned(), json!(self.number));
        value.insert("title".to_owned(), json!(self.title));
        value.insert("body".to_owned(), json!(self.body));
        value.insert(
            PULL_REQUEST_PRIMARY_BRANCH_FIELD.to_owned(),
            json!(self.head_ref_name),
        );
        value.insert(
            super::COMPAT_BRANCH_FIELD.to_owned(),
            json!(self.head_ref_name),
        );
        value.insert("url".to_owned(), json!(self.url));
        Value::Object(value)
    }
}

/// GitHub issue fields used by prompt rendering and launch flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubIssue {
    /// Issue number.
    pub number: u64,
    /// Issue title.
    pub title: String,
    /// Issue body.
    pub body: String,
    /// Browser URL for the issue.
    pub url: String,
    /// Optional branch associated with the issue.
    pub branch: Option<String>,
}

/// A GitHub label name and its display color.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubLabel {
    /// Label name.
    pub name: String,
    /// Hex color without a leading `#`, as emitted by `gh` (for example `d73a4a`).
    #[serde(default)]
    pub color: String,
}

/// Pull request status summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStatus {
    /// Review decision reported by GitHub.
    pub review_decision: ReviewDecision,
    /// Check runs reported for the pull request.
    pub checks: Vec<CheckRun>,
}

/// One GitHub pull request check run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    /// Check run name.
    pub name: String,
    /// Check run status.
    pub status: String,
    /// Optional check conclusion.
    pub conclusion: Option<String>,
    /// Optional browser URL for the check details.
    pub details_url: Option<String>,
}

/// Aggregate outcome of a pull request's check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    /// At least one check ran and none failed or remain pending.
    Passing,
    /// At least one check failed, errored, or was cancelled.
    Failing,
    /// At least one check is still running and none failed.
    Pending,
    /// No checks are reported.
    None,
}

/// Counts of passing, failing, and pending pull request check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CheckSummary {
    /// Checks that concluded successfully.
    pub passed: u32,
    /// Checks that failed, errored, or were cancelled.
    pub failed: u32,
    /// Checks that are queued or still running.
    pub pending: u32,
}

impl CheckSummary {
    /// Summarizes check runs into pass, fail, and pending counts.
    #[must_use]
    pub fn from_checks(checks: &[CheckRun]) -> Self {
        let mut summary = Self::default();
        for check in checks {
            // `gh pr checks` reports lowercase buckets (`pass`/`fail`) while
            // `statusCheckRollup` reports GraphQL enums (`SUCCESS`/`FAILURE`/...);
            // prefer the conclusion when present and fall back to the raw status.
            match check.conclusion.as_deref().unwrap_or(check.status.as_str()) {
                "pass" | "SUCCESS" | "COMPLETED" => summary.passed += 1,
                "fail" | "FAILURE" | "ERROR" | "cancel" => summary.failed += 1,
                _ => summary.pending += 1,
            }
        }
        summary
    }

    /// Returns the total number of reported checks.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.pending
    }

    /// Returns the overall CI state implied by these counts.
    #[must_use]
    pub fn state(&self) -> CiState {
        if self.failed > 0 {
            CiState::Failing
        } else if self.pending > 0 {
            CiState::Pending
        } else if self.passed > 0 {
            CiState::Passing
        } else {
            CiState::None
        }
    }
}

/// Review decision reported by GitHub for a pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReviewDecision {
    /// The pull request has been approved.
    Approved,
    /// A reviewer requested changes.
    ChangesRequested,
    /// A review is required before merge.
    ReviewRequired,
    /// GitHub returned no review decision.
    #[default]
    None,
    /// GitHub returned a value this client does not yet classify.
    Unknown(String),
}

impl Serialize for ReviewDecision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Approved => serializer.serialize_str("APPROVED"),
            Self::ChangesRequested => serializer.serialize_str("CHANGES_REQUESTED"),
            Self::ReviewRequired => serializer.serialize_str("REVIEW_REQUIRED"),
            Self::None => serializer.serialize_none(),
            Self::Unknown(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for ReviewDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Option::<String>::deserialize(deserializer)?;
        Ok(match value.as_deref() {
            Some("APPROVED") => Self::Approved,
            Some("CHANGES_REQUESTED") => Self::ChangesRequested,
            Some("REVIEW_REQUIRED") => Self::ReviewRequired,
            Some(other) => Self::Unknown(other.to_owned()),
            None => Self::None,
        })
    }
}

/// Errors raised by the GitHub provider client.
#[derive(Debug, Error)]
pub enum GitHubError {
    /// The GitHub CLI is unavailable.
    #[error("GitHub CLI `gh` is not available: {message}")]
    MissingGh {
        /// Redacted startup failure message.
        message: String,
    },
    /// The GitHub CLI exited unsuccessfully.
    #[error("GitHub CLI command failed with status {status}: {stderr}")]
    GhFailed {
        /// Arguments passed to `gh`.
        args: Vec<String>,
        /// Process exit status.
        status: i32,
        /// Redacted standard error.
        stderr: String,
    },
    /// The GitHub CLI output was not valid JSON for the requested shape.
    #[error("GitHub CLI returned invalid JSON for {command}: {source}")]
    InvalidJson {
        /// Logical command being parsed.
        command: &'static str,
        /// Underlying JSON decoding failure.
        source: serde_json::Error,
    },
    /// The GitHub CLI output could not be decoded as UTF-8.
    #[error("GitHub CLI returned non-UTF-8 {stream}: {source}")]
    InvalidUtf8 {
        /// Output stream name.
        stream: &'static str,
        /// Underlying UTF-8 decoding failure.
        source: std::string::FromUtf8Error,
    },
    /// The GitHub CLI command exceeded the configured timeout.
    #[error("GitHub CLI command timed out after {timeout:?}")]
    GhTimedOut {
        /// Arguments passed to `gh`.
        args: Vec<String>,
        /// Configured timeout.
        timeout: Duration,
    },
}

impl GitHubError {
    /// Creates a missing-`gh` error with a redacted message.
    #[must_use]
    pub fn missing_gh(message: impl Into<String>) -> Self {
        Self::MissingGh {
            message: message.into(),
        }
    }

    /// Returns whether this error means the GitHub CLI is unavailable.
    #[must_use]
    pub fn is_missing_gh(&self) -> bool {
        matches!(self, Self::MissingGh { .. })
    }
}

/// GitHub provider client.
pub struct GitHubClient<R> {
    runner: R,
}

impl<R> std::fmt::Debug for GitHubClient<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubClient")
            .field("runner", &"<gh runner>")
            .finish()
    }
}

impl<R> Clone for GitHubClient<R>
where
    R: Clone,
{
    fn clone(&self) -> Self {
        Self {
            runner: self.runner.clone(),
        }
    }
}

impl<R> GitHubClient<R> {
    /// Creates a GitHub client from an explicit runner.
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Returns this client's runner.
    #[must_use]
    pub fn runner(&self) -> &R {
        &self.runner
    }
}

impl GitHubClient<CommandGhRunner> {
    /// Creates a production GitHub client that shells out to `gh`.
    #[must_use]
    pub fn with_config(config: GitHubConfig) -> Self {
        Self::new(CommandGhRunner::new(config))
    }
}

impl<R> GitHubClient<R>
where
    R: GhRunner,
{
    /// Lists GitHub pull requests for the current repository.
    ///
    /// `filter_args` are extra `gh pr list` arguments (such as `--state` and
    /// `--search`) selecting a named filter view; pass an empty slice for the
    /// default open listing. See [`crate::providers::filters::GitHubFilter::gh_args`].
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn list_pull_requests(
        &self,
        filter_args: &[String],
    ) -> Result<Vec<GitHubPullRequest>, GitHubError> {
        let mut command = args(["pr", "list", "--json", PR_FIELDS]);
        command.extend_from_slice(filter_args);
        let output = self.run_gh("pr list", command).await?;
        parse_json("pr list", &output.stdout)
    }

    /// Lists GitHub issues for the current repository.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn list_issues(&self) -> Result<Vec<GitHubIssue>, GitHubError> {
        let output = self
            .run_gh(
                "issue list",
                args(["issue", "list", "--json", ISSUE_FIELDS]),
            )
            .await?;
        parse_json::<Vec<GhIssue>>("issue list", &output.stdout).map(|issues| {
            issues
                .into_iter()
                .map(GitHubIssue::from)
                .collect::<Vec<_>>()
        })
    }

    /// Fetches one GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn pull_request(&self, number: u64) -> Result<GitHubPullRequest, GitHubError> {
        let number = number.to_string();
        let output = self
            .run_gh(
                "pr view",
                vec![
                    "pr".to_owned(),
                    "view".to_owned(),
                    number,
                    "--json".to_owned(),
                    PR_FIELDS.to_owned(),
                ],
            )
            .await?;
        parse_json("pr view", &output.stdout)
    }

    /// Fetches check runs for one GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn pull_request_checks(&self, number: u64) -> Result<Vec<CheckRun>, GitHubError> {
        let number = number.to_string();
        let output = self
            .run_gh_with_exit_policy(
                "pr checks",
                vec![
                    "pr".to_owned(),
                    "checks".to_owned(),
                    number,
                    "--json".to_owned(),
                    CHECK_FIELDS.to_owned(),
                ],
                GhExitPolicy::AllowPendingChecks,
            )
            .await?;
        parse_json::<Vec<GhCheckRun>>("pr checks", &output.stdout)
            .map(|checks| checks.into_iter().map(CheckRun::from).collect::<Vec<_>>())
    }

    /// Fetches the review decision for one GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn pull_request_review_decision(
        &self,
        number: u64,
    ) -> Result<ReviewDecision, GitHubError> {
        let number = number.to_string();
        let output = self
            .run_gh(
                "pr view reviewDecision",
                vec![
                    "pr".to_owned(),
                    "view".to_owned(),
                    number,
                    "--json".to_owned(),
                    REVIEW_FIELDS.to_owned(),
                ],
            )
            .await?;
        parse_json::<GhReviewDecision>("pr view reviewDecision", &output.stdout)
            .map(|decision| decision.review_decision)
    }

    /// Fetches the unified diff for one GitHub pull request.
    ///
    /// Output is `gh pr diff`'s raw unified-diff text, not JSON — the same
    /// format the shared diff parser (`crate::parse_unified_diff`) also
    /// consumes from `session.diff`, so one parser serves both sources. No
    /// token handling: authentication is delegated entirely to `gh`.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, or
    /// invalid UTF-8 output.
    pub async fn pull_request_diff(&self, number: u64) -> Result<String, GitHubError> {
        let number = number.to_string();
        let output = self
            .run_gh("pr diff", vec!["pr".to_owned(), "diff".to_owned(), number])
            .await?;
        Ok(output.stdout)
    }

    /// Fetches review and check status for one GitHub pull request.
    ///
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn pull_request_status(&self, number: u64) -> Result<PullRequestStatus, GitHubError> {
        let review_decision = self.pull_request_review_decision(number).await?;
        let checks = self.pull_request_checks(number).await?;
        Ok(PullRequestStatus {
            review_decision,
            checks,
        })
    }

    async fn run_gh(
        &self,
        command: &'static str,
        args: Vec<String>,
    ) -> Result<GhOutput, GitHubError> {
        self.run_gh_with_exit_policy(command, args, GhExitPolicy::SuccessOnly)
            .await
    }

    async fn run_gh_with_exit_policy(
        &self,
        _command: &'static str,
        args: Vec<String>,
        exit_policy: GhExitPolicy,
    ) -> Result<GhOutput, GitHubError> {
        let output = self.runner.run(args.clone()).await?;
        if exit_policy.accepts(output.status) {
            Ok(output)
        } else {
            Err(GitHubError::GhFailed {
                args,
                status: output.status,
                stderr: redacted_stderr(&output.stderr),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhExitPolicy {
    SuccessOnly,
    AllowPendingChecks,
}

impl GhExitPolicy {
    const fn accepts(self, status: i32) -> bool {
        status == 0
            || matches!(self, Self::AllowPendingChecks) && status == PENDING_CHECKS_EXIT_CODE
    }
}

/// GitHub CLI runner backed by a cancellable async process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandGhRunner {
    config: GitHubConfig,
}

impl CommandGhRunner {
    /// Creates a command-backed GitHub CLI runner.
    #[must_use]
    pub fn new(config: GitHubConfig) -> Self {
        Self { config }
    }

    /// Returns this runner's configuration.
    #[must_use]
    pub fn config(&self) -> &GitHubConfig {
        &self.config
    }
}

impl GhRunner for CommandGhRunner {
    fn run(&self, args: Vec<String>) -> GhFuture<'_> {
        Box::pin(async move {
            let mut command = Command::new(&self.config.gh_bin);
            command
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if let Some(repo_cwd) = &self.config.repo_cwd {
                command.current_dir(repo_cwd);
            }

            let child = command.spawn().map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    GitHubError::missing_gh(source.to_string())
                } else {
                    GitHubError::GhFailed {
                        args: args.clone(),
                        status: -1,
                        stderr: redacted_stderr(&source.to_string()),
                    }
                }
            })?;

            let output = tokio::time::timeout(self.config.timeout, child.wait_with_output())
                .await
                .map_err(|_elapsed| GitHubError::GhTimedOut {
                    args: args.clone(),
                    timeout: self.config.timeout,
                })?
                .map_err(|source| GitHubError::GhFailed {
                    args: args.clone(),
                    status: -1,
                    stderr: redacted_stderr(&source.to_string()),
                })?;

            let stdout =
                String::from_utf8(output.stdout).map_err(|source| GitHubError::InvalidUtf8 {
                    stream: "stdout",
                    source,
                })?;
            let stderr =
                String::from_utf8(output.stderr).map_err(|source| GitHubError::InvalidUtf8 {
                    stream: "stderr",
                    source,
                })?;
            Ok(GhOutput {
                status: output.status.code().unwrap_or(-1),
                stdout,
                stderr,
            })
        })
    }
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    url: String,
}

impl From<GhIssue> for GitHubIssue {
    fn from(issue: GhIssue) -> Self {
        Self {
            number: issue.number,
            title: issue.title,
            body: issue.body.unwrap_or_default(),
            url: issue.url,
            branch: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GhCheckRun {
    name: String,
    state: String,
    bucket: Option<String>,
    link: Option<String>,
}

impl From<GhCheckRun> for CheckRun {
    fn from(check: GhCheckRun) -> Self {
        Self {
            name: check.name,
            status: check.state,
            conclusion: non_empty_string(check.bucket),
            details_url: non_empty_string(check.link),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GhReviewDecision {
    #[serde(rename = "reviewDecision")]
    review_decision: ReviewDecision,
}

#[derive(Debug, Deserialize)]
struct GhAuthor {
    #[serde(default)]
    login: Option<String>,
}

/// Extracts the author login from the `gh` author object, dropping empties.
fn author_login<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let author = Option::<GhAuthor>::deserialize(deserializer)?;
    Ok(author.and_then(|author| non_empty_string(author.login)))
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
    #[serde(default)]
    color: String,
}

/// Maps the `gh` label array to [`GitHubLabel`] values.
fn labels_from_gh<'de, D>(deserializer: D) -> Result<Vec<GitHubLabel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let labels = Vec::<GhLabel>::deserialize(deserializer)?;
    Ok(labels
        .into_iter()
        .map(|label| GitHubLabel {
            name: label.name,
            color: label.color,
        })
        .collect())
}

/// One `statusCheckRollup` entry, covering both the `CheckRun` and
/// `StatusContext` GraphQL shapes that `gh` returns.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRollupEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    details_url: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

impl From<GhRollupEntry> for CheckRun {
    fn from(entry: GhRollupEntry) -> Self {
        Self {
            // `CheckRun` carries `name`/`status`/`conclusion`; `StatusContext`
            // carries `context`/`state` and no separate conclusion.
            name: entry.name.or(entry.context).unwrap_or_default(),
            status: entry.status.or(entry.state).unwrap_or_default(),
            conclusion: non_empty_string(entry.conclusion),
            details_url: non_empty_string(entry.details_url.or(entry.target_url)),
        }
    }
}

/// Maps the `statusCheckRollup` array to [`CheckRun`] values.
fn checks_from_rollup<'de, D>(deserializer: D) -> Result<Vec<CheckRun>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries = Vec::<GhRollupEntry>::deserialize(deserializer)?;
    Ok(entries.into_iter().map(CheckRun::from).collect())
}

fn parse_json<T>(command: &'static str, raw: &str) -> Result<T, GitHubError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(raw).map_err(|source| GitHubError::InvalidJson { command, source })
}

fn args<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.into_iter().map(ToOwned::to_owned).collect()
}

fn non_empty_string(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn redacted_stderr(stderr: &str) -> String {
    redact_auth_tokens(stderr.trim())
}

fn redact_auth_tokens(value: &str) -> String {
    let redacted_prefixes = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];
    let mut redacted = redact_bearer_tokens(value);
    for prefix in redacted_prefixes {
        redacted = redact_prefixed_token(&redacted, prefix);
    }
    redacted
}

fn redact_prefixed_token(value: &str, prefix: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find(prefix) {
        output.push_str(&rest[..index]);
        output.push_str("[redacted]");
        let token_start = index + prefix.len();
        let token_len = rest[token_start..]
            .chars()
            .take_while(|value| is_token_char(*value))
            .map(char::len_utf8)
            .sum::<usize>();
        rest = &rest[token_start + token_len..];
    }
    output.push_str(rest);
    output
}

fn redact_bearer_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.to_ascii_lowercase().find("bearer ") {
        output.push_str(&rest[..index]);
        output.push_str("Bearer [redacted]");
        let token_start = index + "bearer ".len();
        let token_len = rest[token_start..]
            .chars()
            .take_while(|value| is_token_char(*value))
            .map(char::len_utf8)
            .sum::<usize>();
        rest = &rest[token_start + token_len..];
    }
    output.push_str(rest);
    output
}

const fn is_token_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '_' | '-')
}

fn empty_string_from_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}
