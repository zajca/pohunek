use std::fs;
use std::io::Write as _;
use std::process::{Command, Stdio};

use pohunek_test_support::env::TestEnv;

fn pohunek(env: &TestEnv) -> Command {
    env.command(pohunek_test_support::bin_exe("pohunek"))
}

#[test]
fn prompt_render_writes_rendered_prompt_without_extra_newline() {
    let env = TestEnv::new().expect("hermetic test environment");
    let template = env.cwd().join("issue.tmpl");
    fs::write(&template, "Issue ${id}: ${title}\n${body}").expect("write template");

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
        .write_all(
            br#"{"identifier":"LIN-1","title":"Fix launcher","description":"Body","branchName":"lin-1"}"#,
        )
        .expect("write stdin");

    let out = child.wait_with_output().expect("wait pohunek");

    assert!(
        out.status.success(),
        "prompt render failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        "Issue LIN-1: Fix launcher\nBody"
    );
    assert!(
        out.stderr.is_empty(),
        "successful render must not write stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
