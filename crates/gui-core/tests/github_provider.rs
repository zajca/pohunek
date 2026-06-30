//! GitHub provider client tests.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::future::Future;
#[cfg(unix)]
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pohunek_gui_core::providers::github::{
    CheckRun, GhOutput, GhRunner, GitHubClient, GitHubConfig, GitHubError, GitHubPullRequest,
    PullRequestStatus, ReviewDecision,
};
use serde_json::json;

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
                "number,title,body,headRefName,url".to_owned(),
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
                "number,title,body,headRefName,url".to_owned(),
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
        GitHubPullRequest {
            number: 7,
            title: "Fix filters".to_owned(),
            body: "Body text".to_owned(),
            head_ref_name: "feature/filters".to_owned(),
            url: "https://github.example/repo/pull/7".to_owned(),
        }
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
    let dir = std::env::temp_dir().join(format!(
        "pohunek-gui-core-gh-provider-{}",
        std::process::id()
    ));
    let script = dir.join("gh");
    let repo = dir.join("repo");
    let seen_cwd = dir.join("seen-cwd");
    std::fs::create_dir_all(&repo).expect("create fake repo cwd");
    let script_body = r#"#!/bin/sh
set -eu
pwd > __SEEN_CWD__
case "$*" in
  "pr list --json number,title,body,headRefName,url")
    printf '%s' '[{"number":7,"title":"Fix filters","body":"Body text","headRefName":"feature/filters","url":"https://github.example/repo/pull/7"}]'
    ;;
  "issue list --json number,title,body,url")
    printf '%s' '[{"number":11,"title":"Crash on launch","body":"Issue body","url":"https://github.example/repo/issues/11"}]'
    ;;
  "pr view 7 --json reviewDecision")
    printf '%s' '{"reviewDecision":"APPROVED"}'
    ;;
  "pr checks 7 --json name,state,bucket,link")
    printf '%s' '[{"name":"test","state":"SUCCESS","bucket":"pass","link":"https://github.example/checks/1"}]'
    ;;
  *)
    printf 'unexpected gh args: %s\n' "$*" >&2
    exit 64
    ;;
esac
"#
    .replace("__SEEN_CWD__", &shell_quote(&seen_cwd));
    write_fake_gh(&script, &script_body);

    let client = GitHubClient::with_config(GitHubConfig::new(&script).with_repo_cwd(&repo));

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
    let dir = std::env::temp_dir().join(format!(
        "pohunek-gui-core-gh-provider-error-{}",
        std::process::id()
    ));
    let script = dir.join("gh");
    write_fake_gh(
        &script,
        r"#!/bin/sh
printf 'authentication required\n' >&2
exit 1
",
    );

    let client = GitHubClient::with_config(GitHubConfig::new(&script));

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
fn write_fake_gh(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(path.parent().expect("fake gh path has parent"))
        .expect("create fake gh dir");
    std::fs::write(path, body).expect("write fake gh script");
    let mut permissions = std::fs::metadata(path)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fake gh script");
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
