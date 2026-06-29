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
use serde_json::{json, Value};
use thiserror::Error;
use tokio::process::Command;

const PR_FIELDS: &str = "number,title,body,headRefName,url";
const ISSUE_FIELDS: &str = "number,title,body,url";
const CHECK_FIELDS: &str = "name,state,bucket,link";
const REVIEW_FIELDS: &str = "reviewDecision";
const DEFAULT_GH_TIMEOUT: Duration = Duration::from_secs(20);
const PENDING_CHECKS_EXIT_CODE: i32 = 8;

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

/// GitHub pull request fields used by prompt rendering and launch flows.
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
}

impl GitHubPullRequest {
    /// Returns the item id used by shared prompt rendering.
    #[must_use]
    pub fn prompt_item_id(&self) -> String {
        self.number.to_string()
    }

    /// Converts this pull request to the shared prompt renderer JSON shape.
    #[must_use]
    pub fn to_prompt_json(&self) -> Value {
        let id = self.prompt_item_id();
        json!({
            "id": id,
            "number": self.number,
            "title": self.title,
            "body": self.body,
            "headRefName": self.head_ref_name,
            "branch": self.head_ref_name,
            "url": self.url,
        })
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

/// Review decision reported by GitHub for a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    /// The pull request has been approved.
    Approved,
    /// A reviewer requested changes.
    ChangesRequested,
    /// A review is required before merge.
    ReviewRequired,
    /// GitHub returned no review decision.
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
    /// # Errors
    ///
    /// Returns a typed [`GitHubError`] for missing `gh`, nonzero exit, invalid
    /// UTF-8, or invalid JSON output.
    pub async fn list_pull_requests(&self) -> Result<Vec<GitHubPullRequest>, GitHubError> {
        let output = self
            .run_gh("pr list", args(["pr", "list", "--json", PR_FIELDS]))
            .await?;
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
