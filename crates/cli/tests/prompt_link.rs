use std::io::Write as _;
use std::process::{Command, Output, Stdio};

/// The built `pohunek` binary with an empty environment: `prompt link` renders
/// from stdin and arguments and reads none of it.
fn pohunek() -> Command {
    let mut command = Command::new(pohunek_test_support::bin_exe("pohunek"));
    command.env_clear();
    command
}

fn run_prompt_link(provider: &str, item_id: &str, url: &str, context_json: &str) -> String {
    let out = run_prompt_link_process(provider, item_id, url, context_json);

    assert!(
        out.status.success(),
        "prompt link failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "successful link render must not write stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn run_prompt_link_process(provider: &str, item_id: &str, url: &str, context_json: &str) -> Output {
    let mut child = pohunek()
        .args([
            "prompt",
            "link",
            "--provider",
            provider,
            "--item-id",
            item_id,
            "--url",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pohunek");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(context_json.as_bytes())
        .expect("write stdin");

    child.wait_with_output().expect("wait pohunek")
}

#[test]
fn prompt_link_writes_linear_metadata_in_canonical_order() {
    let stdout = run_prompt_link(
        "linear_issue",
        "LIN-123",
        "https://linear.test/LIN-123",
        r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}"#,
    );

    assert_eq!(
        stdout,
        "link.branch=lin-123-fix-launcher\n\
         link.id=LIN-123\n\
         link.kind=issue\n\
         link.provider=linear\n\
         link.url=https://linear.test/LIN-123\n"
    );
}

#[test]
fn prompt_link_writes_github_metadata_in_canonical_order() {
    let stdout = run_prompt_link(
        "github_pr",
        "7",
        "https://example.test/pr/7",
        r#"{"number":7,"title":"Fix filters","body":"Body text","headRefName":"feature/filters","url":"https://example.test/pr/7"}"#,
    );

    assert_eq!(
        stdout,
        "link.branch=feature/filters\n\
         link.id=7\n\
         link.kind=pull_request\n\
         link.provider=github\n\
         link.url=https://example.test/pr/7\n"
    );
}

#[test]
fn prompt_link_invalid_json_honors_json_error_output() {
    let mut child = pohunek()
        .args([
            "prompt",
            "link",
            "--provider",
            "linear_issue",
            "--item-id",
            "LIN-1",
            "--url",
            "https://linear.test/LIN-1",
            "--json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pohunek");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{")
        .expect("write stdin");

    let out = child.wait_with_output().expect("wait pohunek");

    assert_eq!(
        out.status.code(),
        Some(1),
        "invalid JSON must exit with the CLI failure code: {out:?}"
    );
    assert!(
        out.stderr.is_empty(),
        "json errors must not write human stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout must be JSON ({err}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "prompt_render_failed");
    assert_eq!(doc["err"]["class"], "configuration");
    assert!(
        doc["err"]["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("provider returned invalid JSON")),
        "error should describe invalid provider JSON: {doc:?}"
    );
}

#[test]
fn prompt_link_rejects_unsafe_provider_branch_values() {
    for (context, expected) in [
        (
            r#"{"title":"Title","branchName":" "}"#,
            "provider link metadata is missing `link.branch`",
        ),
        (
            "{\"title\":\"Title\",\"branchName\":\"feature/line\\nbreak\"}",
            "provider link metadata `link.branch` contains an ASCII control character",
        ),
        (
            r#"{"title":"Title","branchName":"feature/ta\tb"}"#,
            "provider link metadata `link.branch` contains an ASCII control character",
        ),
        (
            r#"{"title":"Title","branchName":"feature/\u007fdelete"}"#,
            "provider link metadata `link.branch` contains an ASCII control character",
        ),
    ] {
        let out = run_prompt_link_process(
            "linear_issue",
            "LIN-1",
            "https://linear.test/LIN-1",
            context,
        );
        assert!(!out.status.success(), "unsafe branch must fail: {out:?}");
        assert!(
            out.stdout.is_empty(),
            "failed link render must not write stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(expected),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `prompt link` validates the CLI-provided identifier and URL the same way as
/// the provider-derived branch: empty or whitespace-only values are reported as
/// missing, values with ASCII control characters are rejected outright.
#[test]
fn prompt_link_rejects_unsafe_item_id_and_url_values() {
    for (row, item_id, url, expected) in [
        (
            "whitespace-only id",
            " ",
            "https://linear.test/LIN-1",
            "provider link metadata is missing `link.id`",
        ),
        (
            "newlines in id",
            "LIN-1\nLIN-2",
            "https://linear.test/LIN-1",
            "provider link metadata `link.id` contains an ASCII control character",
        ),
        (
            "whitespace-only url",
            "LIN-1",
            " \t ",
            "provider link metadata is missing `link.url`",
        ),
        (
            "vertical tab in url",
            "LIN-1",
            "https://linear.test/LIN-1\u{0b}bad",
            "provider link metadata `link.url` contains an ASCII control character",
        ),
    ] {
        let out = run_prompt_link_process(
            "linear_issue",
            item_id,
            url,
            r#"{"title":"Fix launcher","branchName":"lin-1-fix-launcher"}"#,
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "{row} must exit with the CLI failure code: {out:?}"
        );
        assert!(
            out.stdout.is_empty(),
            "{row} must not write stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected),
            "{row} must name the invalid link field on stderr: {stderr}"
        );
    }
}

/// GitHub picks `headRefName` first, so a provider that supplies several branch
/// fields still resolves the canonical head branch.
#[test]
fn prompt_link_uses_github_head_ref_name_precedence() {
    let stdout = run_prompt_link(
        "github_pr",
        "7",
        "https://example.test/pr/7",
        r#"{"title":"Fix filters","headRefName":"feature/head","branch":"feature/branch","branchName":"feature/branch-name"}"#,
    );
    assert!(
        stdout.contains("link.branch=feature/head\n"),
        "headRefName must win over the fallback fields: {stdout}"
    );
}

/// With no `headRefName` or `branch`, the last GitHub fallback field is used.
#[test]
fn prompt_link_uses_github_branch_name_final_fallback() {
    let stdout = run_prompt_link(
        "github_pr",
        "7",
        "https://example.test/pr/7",
        r#"{"title":"Fix filters","branchName":"feature/branch-name"}"#,
    );
    assert!(
        stdout.contains("link.branch=feature/branch-name\n"),
        "branchName must be accepted as the final GitHub fallback: {stdout}"
    );
}

/// Linear prefers `branchName`, unlike GitHub's `headRefName`-first order.
#[test]
fn prompt_link_uses_linear_branch_name_precedence() {
    let stdout = run_prompt_link(
        "linear_issue",
        "LIN-1",
        "https://linear.test/LIN-1",
        r#"{"title":"Fix launcher","branchName":"lin-1-fix-launcher","branch":"feature/branch"}"#,
    );
    assert!(
        stdout.contains("link.branch=lin-1-fix-launcher\n"),
        "branchName must win over the fallback field for Linear: {stdout}"
    );
}

#[test]
fn prompt_link_uses_github_branch_fallback() {
    let stdout = run_prompt_link(
        "github_pr",
        "7",
        "https://example.test/pr/7",
        r#"{"title":"Fix filters","branch":"feature/fallback","branchName":"feature/other"}"#,
    );
    assert!(
        stdout.contains("link.branch=feature/fallback\n"),
        "{stdout}"
    );
}

#[test]
fn prompt_link_uses_linear_branch_fallback() {
    let stdout = run_prompt_link(
        "linear_issue",
        "LIN-1",
        "https://linear.test/LIN-1",
        r#"{"title":"Fix launcher","branch":"feature/linear-fallback"}"#,
    );
    assert!(
        stdout.contains("link.branch=feature/linear-fallback\n"),
        "{stdout}"
    );
}
