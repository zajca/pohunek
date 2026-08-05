//! GitHub provider client tests.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::future::Future;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::pin::Pin;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pohunek_gui_core::providers::github::{
    CheckRun, CheckSummary, CiState, GhOutput, GhRunner, GitHubClient, GitHubConfig, GitHubError,
    GitHubLabel, GitHubPullRequest, PullRequestStatus, ReviewDecision,
};
use serde_json::json;

#[cfg(unix)]
static NEXT_FAKE_GH_DIR: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedGhCall {
    args: Vec<String>,
}

#[derive(Debug, Clone)]
struct FakeGhRunner {
    calls: Arc<Mutex<Vec<RecordedGhCall>>>,
    responses: Arc<Mutex<Vec<Result<GhOutput, GitHubError>>>>,
}

impl FakeGhRunner {
    fn new(responses: Vec<Result<GhOutput, GitHubError>>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
        }
    }

    fn json(stdout: impl Into<String>) -> GhOutput {
        GhOutput {
            status: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn nonzero(stderr: impl Into<String>) -> GhOutput {
        GhOutput {
            status: 1,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    fn status(status: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> GhOutput {
        GhOutput {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn calls(&self) -> Vec<RecordedGhCall> {
        self.calls.lock().expect("gh calls lock").clone()
    }
}

impl GhRunner for FakeGhRunner {
    fn run(
        &self,
        args: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<GhOutput, GitHubError>> + Send + '_>> {
        let calls = Arc::clone(&self.calls);
        let responses = Arc::clone(&self.responses);
        Box::pin(async move {
            calls
                .lock()
                .expect("gh calls lock")
                .push(RecordedGhCall { args });
            responses.lock().expect("gh responses lock").remove(0)
        })
    }
}

fn client_with_json(stdout: impl Into<String>) -> GitHubClient<FakeGhRunner> {
    GitHubClient::new(FakeGhRunner::new(vec![Ok(FakeGhRunner::json(stdout))]))
}

fn assert_send<T: Send>(_: T) {}

#[tokio::test]
async fn list_pull_requests_shells_out_with_prompt_fields() {
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::json(
        r#"[
            {
                "number": 7,
                "title": "Fix filters",
                "body": "Body text",
                "headRefName": "feature/filters",
                "url": "https://github.example/repo/pull/7"
            }
        ]"#,
    ))]);
    let client = GitHubClient::new(runner.clone());
    assert_send(client.list_pull_requests(&[]));

    let prs = client.list_pull_requests(&[]).await.expect("pull requests");

    assert_eq!(
        runner.calls(),
        vec![RecordedGhCall {
            args: vec![
                "pr".to_owned(),
                "list".to_owned(),
                "--json".to_owned(),
                "number,title,body,headRefName,url,author,isDraft,labels,reviewDecision,statusCheckRollup,additions,deletions,updatedAt".to_owned(),
            ],
        }]
    );
    assert_eq!(prs.len(), 1);
    let pr = &prs[0];
    assert_eq!(pr.number, 7);
    assert_eq!(pr.title, "Fix filters");
    assert_eq!(pr.body, "Body text");
    assert_eq!(pr.head_ref_name, "feature/filters");
    assert_eq!(pr.url, "https://github.example/repo/pull/7");
    assert_eq!(pr.prompt_item_id(), "7");
}

#[tokio::test]
async fn list_pull_requests_parses_list_metadata() {
    // Mixed rollup: a `CheckRun` (status/conclusion) and a `StatusContext`
    // (context/state), plus author, draft flag, labels, and diff size.
    let client = client_with_json(
        r#"[
            {
                "number": 7,
                "title": "Fix filters",
                "body": "Body text",
                "headRefName": "feature/filters",
                "url": "https://github.example/repo/pull/7",
                "author": {"login": "octocat"},
                "isDraft": true,
                "labels": [
                    {"name": "bug", "color": "d73a4a"},
                    {"name": "ui", "color": ""}
                ],
                "reviewDecision": "APPROVED",
                "statusCheckRollup": [
                    {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://ci.example/build"},
                    {"__typename": "StatusContext", "context": "legacy", "state": "FAILURE", "targetUrl": "https://ci.example/legacy"},
                    {"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": null}
                ],
                "additions": 84,
                "deletions": 12,
                "updatedAt": "2026-06-28T10:00:00Z"
            }
        ]"#,
    );

    let prs = client.list_pull_requests(&[]).await.expect("pull requests");
    let pr = &prs[0];

    assert_eq!(pr.author.as_deref(), Some("octocat"));
    assert!(pr.is_draft);
    assert_eq!(
        pr.labels,
        vec![
            GitHubLabel {
                name: "bug".to_owned(),
                color: "d73a4a".to_owned(),
            },
            GitHubLabel {
                name: "ui".to_owned(),
                color: String::new(),
            },
        ]
    );
    assert_eq!(pr.review_decision, ReviewDecision::Approved);
    assert_eq!(pr.additions, 84);
    assert_eq!(pr.deletions, 12);
    assert_eq!(pr.updated_at.as_deref(), Some("2026-06-28T10:00:00Z"));

    let summary = CheckSummary::from_checks(&pr.checks);
    assert_eq!(summary.passed, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.total(), 3);
    assert_eq!(summary.state(), CiState::Failing);
}

#[test]
fn check_summary_state_prefers_failing_then_pending() {
    assert_eq!(CheckSummary::default().state(), CiState::None);
    assert_eq!(
        CheckSummary {
            passed: 3,
            failed: 0,
            pending: 0,
        }
        .state(),
        CiState::Passing
    );
    assert_eq!(
        CheckSummary {
            passed: 2,
            failed: 0,
            pending: 1,
        }
        .state(),
        CiState::Pending
    );
    assert_eq!(
        CheckSummary {
            passed: 2,
            failed: 1,
            pending: 1,
        }
        .state(),
        CiState::Failing
    );
}

#[tokio::test]
async fn list_pull_requests_tolerates_missing_metadata() {
    // Older/minimal `gh` output without the new fields still parses.
    let client = client_with_json(
        r#"[
            {
                "number": 7,
                "title": "Fix filters",
                "body": "Body text",
                "headRefName": "feature/filters",
                "url": "https://github.example/repo/pull/7"
            }
        ]"#,
    );

    let prs = client.list_pull_requests(&[]).await.expect("pull requests");
    let pr = &prs[0];

    assert_eq!(pr.author, None);
    assert!(!pr.is_draft);
    assert!(pr.labels.is_empty());
    assert_eq!(pr.review_decision, ReviewDecision::None);
    assert!(pr.checks.is_empty());
    assert_eq!(pr.additions, 0);
    assert_eq!(pr.updated_at, None);
}

#[tokio::test]
async fn list_pull_requests_appends_filter_args() {
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::json("[]"))]);
    let client = GitHubClient::new(runner.clone());

    let filter_args = vec![
        "--state".to_owned(),
        "open".to_owned(),
        "--search".to_owned(),
        "review:approved".to_owned(),
    ];
    client
        .list_pull_requests(&filter_args)
        .await
        .expect("filtered pull requests");

    assert_eq!(
        runner.calls(),
        vec![RecordedGhCall {
            args: vec![
                "pr".to_owned(),
                "list".to_owned(),
                "--json".to_owned(),
                "number,title,body,headRefName,url,author,isDraft,labels,reviewDecision,statusCheckRollup,additions,deletions,updatedAt".to_owned(),
                "--state".to_owned(),
                "open".to_owned(),
                "--search".to_owned(),
                "review:approved".to_owned(),
            ],
        }]
    );
}

#[tokio::test]
async fn list_issues_shells_out_with_prompt_fields() {
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::json(
        r#"[
            {
                "number": 11,
                "title": "Crash on launch",
                "body": "Issue body",
                "url": "https://github.example/repo/issues/11"
            }
        ]"#,
    ))]);
    let client = GitHubClient::new(runner.clone());

    let issues = client.list_issues().await.expect("issues");

    assert_eq!(
        runner.calls(),
        vec![RecordedGhCall {
            args: vec![
                "issue".to_owned(),
                "list".to_owned(),
                "--json".to_owned(),
                "number,title,body,url".to_owned(),
            ],
        }]
    );
    assert_eq!(issues.len(), 1);
    let issue = &issues[0];
    assert_eq!(issue.number, 11);
    assert_eq!(issue.title, "Crash on launch");
    assert_eq!(issue.body, "Issue body");
    assert_eq!(issue.url, "https://github.example/repo/issues/11");
    assert_eq!(issue.branch, None);
}

#[tokio::test]
async fn pull_request_detail_maps_to_prompt_json_shape() {
    let client = client_with_json(
        r#"{
            "number": 7,
            "title": "Fix filters",
            "body": "Body text",
            "headRefName": "feature/filters",
            "url": "https://github.example/repo/pull/7"
        }"#,
    );

    let pr = client.pull_request(7).await.expect("pull request");

    assert_eq!(
        pr,
        GitHubPullRequest::new(
            7,
            "Fix filters",
            "Body text",
            "feature/filters",
            "https://github.example/repo/pull/7",
        )
    );
    assert_eq!(
        pr.to_prompt_json(),
        json!({
            "id": "7",
            "number": 7,
            "title": "Fix filters",
            "body": "Body text",
            "headRefName": "feature/filters",
            "branch": "feature/filters",
            "url": "https://github.example/repo/pull/7",
        })
    );
}

#[tokio::test]
async fn pull_request_detail_accepts_null_body() {
    let client = client_with_json(
        r#"{
            "number": 7,
            "title": "Fix filters",
            "body": null,
            "headRefName": "feature/filters",
            "url": "https://github.example/repo/pull/7"
        }"#,
    );

    let pr = client.pull_request(7).await.expect("pull request");

    assert_eq!(pr.body, "");
}

#[tokio::test]
async fn pull_request_checks_include_check_run_statuses() {
    let client = client_with_json(
        r#"[
            {
                "name": "test",
                "state": "SUCCESS",
                "bucket": "pass",
                "link": "https://github.example/checks/1"
            },
            {
                "name": "lint",
                "state": "PENDING",
                "bucket": "pending",
                "link": ""
            }
        ]"#,
    );

    let checks = client.pull_request_checks(7).await.expect("checks");

    assert_eq!(
        checks,
        vec![
            CheckRun {
                name: "test".to_owned(),
                status: "SUCCESS".to_owned(),
                conclusion: Some("pass".to_owned()),
                details_url: Some("https://github.example/checks/1".to_owned()),
            },
            CheckRun {
                name: "lint".to_owned(),
                status: "PENDING".to_owned(),
                conclusion: Some("pending".to_owned()),
                details_url: None,
            },
        ]
    );
}

#[tokio::test]
async fn pull_request_diff_shells_out_and_returns_raw_unified_diff_text() {
    let diff_text = "diff --git a/src/lib.rs b/src/lib.rs\n\
                      index abc123..def456 100644\n\
                      --- a/src/lib.rs\n\
                      +++ b/src/lib.rs\n\
                      @@ -1,1 +1,1 @@\n\
                      -old\n\
                      +new\n";
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::json(diff_text))]);
    let client = GitHubClient::new(runner.clone());

    let diff = client.pull_request_diff(7).await.expect("pr diff");

    assert_eq!(
        runner.calls(),
        vec![RecordedGhCall {
            args: vec!["pr".to_owned(), "diff".to_owned(), "7".to_owned()],
        }]
    );
    assert_eq!(diff, diff_text);
}

#[tokio::test]
async fn pull_request_diff_nonzero_gh_exit_is_typed_and_graceful() {
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::nonzero("no such pull request"))]);
    let client = GitHubClient::new(runner);

    let err = client
        .pull_request_diff(999)
        .await
        .expect_err("nonzero gh exit");

    assert!(matches!(err, GitHubError::GhFailed { status: 1, .. }));
}

#[tokio::test]
async fn pull_request_status_includes_review_decision_and_checks() {
    let runner = FakeGhRunner::new(vec![
        Ok(FakeGhRunner::json(
            r#"{ "reviewDecision": "CHANGES_REQUESTED" }"#,
        )),
        Ok(FakeGhRunner::json(
            r#"[
                {
                    "name": "test",
                    "state": "FAILURE",
                    "bucket": "fail",
                    "link": "https://github.example/checks/2"
                }
            ]"#,
        )),
    ]);
    let client = GitHubClient::new(runner);

    let status = client.pull_request_status(7).await.expect("status");

    assert_eq!(
        status,
        PullRequestStatus {
            review_decision: ReviewDecision::ChangesRequested,
            checks: vec![CheckRun {
                name: "test".to_owned(),
                status: "FAILURE".to_owned(),
                conclusion: Some("fail".to_owned()),
                details_url: Some("https://github.example/checks/2".to_owned()),
            }],
        }
    );
}

#[tokio::test]
async fn missing_gh_error_is_typed_and_graceful() {
    let runner = FakeGhRunner::new(vec![Err(GitHubError::missing_gh("gh not found"))]);
    let client = GitHubClient::new(runner);

    let err = client
        .list_pull_requests(&[])
        .await
        .expect_err("missing gh error");

    assert!(err.is_missing_gh());
    assert_eq!(
        err.to_string(),
        "GitHub CLI `gh` is not available: gh not found"
    );
}

#[tokio::test]
async fn nonzero_gh_exit_is_typed_and_graceful() {
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::nonzero(
        "HTTP 401: authentication required for ghp_secretFixture and Bearer github_pat_fixture",
    ))]);
    let client = GitHubClient::new(runner);

    let err = client
        .list_pull_requests(&[])
        .await
        .expect_err("nonzero gh exit");

    assert!(matches!(
        err,
        GitHubError::GhFailed {
            status: 1,
            ref stderr,
            ..
        } if stderr == "HTTP 401: authentication required for [redacted] and Bearer [redacted]"
    ));
    assert!(!err.to_string().contains("ghp_secretFixture"));
    assert!(!err.to_string().contains("github_pat_fixture"));
}

#[tokio::test]
async fn pending_pr_checks_exit_code_still_parses_json() {
    let runner = FakeGhRunner::new(vec![Ok(FakeGhRunner::status(
        8,
        r#"[
            {
                "name": "test",
                "state": "PENDING",
                "bucket": "pending",
                "link": "https://github.example/checks/3"
            }
        ]"#,
        "checks pending",
    ))]);
    let client = GitHubClient::new(runner);

    let checks = client
        .pull_request_checks(7)
        .await
        .expect("pending checks parse");

    assert_eq!(
        checks,
        vec![CheckRun {
            name: "test".to_owned(),
            status: "PENDING".to_owned(),
            conclusion: Some("pending".to_owned()),
            details_url: Some("https://github.example/checks/3".to_owned()),
        }]
    );
}

#[tokio::test]
async fn invalid_json_error_is_typed() {
    let client = client_with_json("{not-json");

    let err = client
        .list_pull_requests(&[])
        .await
        .expect_err("invalid JSON");

    assert!(matches!(err, GitHubError::InvalidJson { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn command_runner_uses_fake_gh_script_for_provider_commands() {
    let dir = fake_gh_dir("gh-provider");
    let repo = dir.join("repo");
    let seen_cwd = dir.join("seen-cwd");
    std::fs::create_dir_all(&repo).expect("create fake repo cwd");
    let client = GitHubClient::with_config(
        GitHubConfig::new(fake_gh_script("fake-gh")).with_repo_cwd(&repo),
    );

    let prs = client
        .list_pull_requests(&[])
        .await
        .expect("fake gh PR list");
    let issues = client.list_issues().await.expect("fake gh issue list");
    let status = client
        .pull_request_status(7)
        .await
        .expect("fake gh PR status");

    assert_eq!(prs[0].number, 7);
    assert_eq!(prs[0].head_ref_name, "feature/filters");
    assert_eq!(issues[0].number, 11);
    assert_eq!(issues[0].branch, None);
    assert_eq!(status.review_decision, ReviewDecision::Approved);
    assert_eq!(
        status.checks,
        vec![CheckRun {
            name: "test".to_owned(),
            status: "SUCCESS".to_owned(),
            conclusion: Some("pass".to_owned()),
            details_url: Some("https://github.example/checks/1".to_owned()),
        }]
    );
    assert_eq!(
        std::fs::read_to_string(seen_cwd).expect("read fake gh cwd"),
        format!("{}\n", repo.display())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn command_runner_fake_gh_error_path_is_typed() {
    let client = GitHubClient::with_config(GitHubConfig::new(fake_gh_script("fake-gh-error")));

    let err = client
        .list_pull_requests(&[])
        .await
        .expect_err("fake gh failure");

    assert!(matches!(
        err,
        GitHubError::GhFailed {
            status: 1,
            ref stderr,
            ..
        } if stderr == "authentication required"
    ));
}

#[tokio::test]
async fn command_runner_missing_gh_is_typed() {
    let client = GitHubClient::with_config(GitHubConfig::new(
        std::env::temp_dir().join("pohunek-gui-core-missing-gh"),
    ));

    let err = client
        .list_pull_requests(&[])
        .await
        .expect_err("missing gh executable");

    assert!(err.is_missing_gh());
}

#[cfg(unix)]
fn fake_gh_script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("assets")
        .join(name)
}

/// Creates a unique temporary directory for one fake GitHub CLI fixture.
#[cfg(unix)]
fn fake_gh_dir(name: &str) -> PathBuf {
    loop {
        // The counter reserves names only; it does not synchronize fixture data.
        let sequence = NEXT_FAKE_GH_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pohunek-gui-core-{name}-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create unique fake gh dir {}: {error}", path.display()),
        }
    }
}
