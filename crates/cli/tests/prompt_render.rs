use std::fs;
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use pohunek_test_support::env::TestEnv;

fn pohunek(env: &TestEnv) -> Command {
    env.command(pohunek_test_support::bin_exe("pohunek"))
}

fn render_with_cli(template_text: &str, context_json: &str) -> Output {
    let env = TestEnv::new().expect("hermetic test environment");
    let template = env.cwd().join("issue.tmpl");
    fs::write(&template, template_text).expect("write template");

    let mut child = pohunek(&env)
        .args([
            "prompt",
            "render",
            "--provider",
            "linear_issue",
            "--item-id",
            "LIN-1",
            "--template-file",
            template.to_str().expect("utf8 template path"),
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
fn prompt_render_writes_rendered_prompt_without_extra_newline() {
    let out = render_with_cli(
        "Issue ${id}: ${title}\n${body}",
        r#"{"identifier":"LIN-1","title":"Fix launcher","description":"Body","branchName":"lin-1"}"#,
    );

    assert!(
        out.status.success(),
        "prompt render failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        "Issue LIN-1: Fix launcher\nBody"
    );
    assert!(out.stderr.is_empty(), "successful render wrote stderr");
}

#[test]
fn prompt_render_preserves_provider_placeholders_as_literal_text() {
    let out = render_with_cli(
        "Title: ${title}\n",
        r#"{"identifier":"LIN-1","title":"${body}","description":"private body","branchName":"lin-1"}"#,
    );

    assert!(
        out.status.success(),
        "prompt render failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        "Title: ${body}\n"
    );
}

#[test]
fn prompt_render_rejects_unknown_variables_and_missing_fields() {
    let unknown = render_with_cli(
        "${z_var} ${a_var}",
        r#"{"identifier":"LIN-1","title":"Title","branchName":"lin-1"}"#,
    );
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unknown.stderr)
        .contains("template references unknown variable(s): a_var, z_var"));

    let missing = render_with_cli("${title}", r#"{"identifier":"LIN-1"}"#);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr)
        .contains("provider JSON missing required field: title"));
}
