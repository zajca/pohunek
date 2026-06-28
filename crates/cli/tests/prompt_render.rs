use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-cli-prompt-{tag}-{}-{nanos}-{n}",
        std::process::id(),
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn pohunek() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pohunek"))
}

#[test]
fn prompt_render_writes_rendered_prompt_without_extra_newline() {
    let dir = temp_dir("render");
    let template = dir.join("issue.tmpl");
    fs::write(&template, "Issue ${id}: ${title}\n${body}").expect("write template");

    let mut child = pohunek()
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

#[test]
fn prompt_render_help_documents_required_inputs() {
    let out = pohunek()
        .args(["prompt", "render", "--help"])
        .output()
        .expect("spawn pohunek");

    assert!(out.status.success(), "help exits successfully");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert!(stdout.contains("--provider"), "{stdout}");
    assert!(stdout.contains("--item-id"), "{stdout}");
    assert!(stdout.contains("--template-file"), "{stdout}");
}
