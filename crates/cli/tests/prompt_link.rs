use std::io::Write as _;
use std::process::{Command, Stdio};

/// The built `pohunek` binary with an empty environment: `prompt link` renders
/// from stdin and arguments and reads none of it.
fn pohunek() -> Command {
    let mut command = Command::new(pohunek_test_support::bin_exe("pohunek"));
    command.env_clear();
    command
}

fn run_prompt_link(provider: &str, item_id: &str, url: &str, context_json: &str) -> String {
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

    let out = child.wait_with_output().expect("wait pohunek");

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

    assert!(
        !out.status.success(),
        "invalid JSON must fail: {}",
        String::from_utf8_lossy(&out.stdout)
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
    ] {
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
            .write_all(context.as_bytes())
            .expect("write stdin");

        let out = child.wait_with_output().expect("wait pohunek");
        assert!(!out.status.success(), "unsafe branch must fail");
        assert!(out.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(expected),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
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
