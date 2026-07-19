use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pohunek_gui_core::{
    preview_prompt_content, PromptContext, PromptProvider, SessionLinkKind, SessionLinkMetadata,
    SessionLinkProvider,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-cli-gui-parity-{tag}-{}-{nanos}-{n}",
        std::process::id(),
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn run_prompt_link(provider: &str, item_id: &str, url: &str, context_json: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pohunek"))
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

fn parse_metadata(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('=').expect("metadata line has =");
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

#[test]
fn gui_prompt_preview_is_byte_identical_to_pohunek_prompt_render() {
    let dir = temp_dir("linear");
    let template = dir.join("issue.tmpl");
    let template_content = "Issue ${id}: ${title}\n${body}\nbranch=${branch}\n";
    let context_json = r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}"#;
    fs::write(&template, template_content).expect("write template");

    let preview = preview_prompt_content(
        "issue",
        template_content,
        &PromptContext {
            provider: PromptProvider::LinearIssue,
            item_id: "LIN-123".to_owned(),
            json: context_json.to_owned(),
        },
    )
    .expect("render GUI preview");

    let mut child = Command::new(env!("CARGO_BIN_EXE_pohunek"))
        .args([
            "prompt",
            "render",
            "--provider",
            "linear_issue",
            "--item-id",
            "LIN-123",
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

    let out = child.wait_with_output().expect("wait pohunek");

    assert!(
        out.status.success(),
        "prompt render failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        preview.rendered
    );
    assert!(
        out.stderr.is_empty(),
        "successful render must not write stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn gui_github_pr_preview_is_byte_identical_to_pohunek_prompt_render() {
    let dir = temp_dir("github-pr");
    let template = dir.join("pr.tmpl");
    let template_content = "PR ${number}: ${title}\n${body}\nbranch=${branch}\nurl=${url}\n";
    let context_json = r#"{"number":7,"title":"Fix filters","body":"Body text","headRefName":"feature/filters","url":"https://github.example/repo/pull/7"}"#;
    fs::write(&template, template_content).expect("write template");

    let preview = preview_prompt_content(
        "pr",
        template_content,
        &PromptContext {
            provider: PromptProvider::GitHubPr,
            item_id: "7".to_owned(),
            json: context_json.to_owned(),
        },
    )
    .expect("render GUI preview");

    let mut child = Command::new(env!("CARGO_BIN_EXE_pohunek"))
        .args([
            "prompt",
            "render",
            "--provider",
            "github_pr",
            "--item-id",
            "7",
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

    let out = child.wait_with_output().expect("wait pohunek");

    assert!(
        out.status.success(),
        "prompt render failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        preview.rendered
    );
    assert!(
        out.stderr.is_empty(),
        "successful render must not write stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn gui_link_metadata_is_byte_identical_to_pohunek_prompt_link() {
    let linear_json = r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}"#;
    let linear_cli = parse_metadata(&run_prompt_link(
        "linear_issue",
        "LIN-123",
        "https://linear.test/LIN-123",
        linear_json,
    ));
    let linear_gui = SessionLinkMetadata::new(
        SessionLinkProvider::Linear,
        SessionLinkKind::Issue,
        "LIN-123",
        "https://linear.test/LIN-123",
        "lin-123-fix-launcher",
    )
    .expect("valid linear metadata")
    .to_session_metadata();
    assert_eq!(linear_cli, linear_gui);

    let github_json = r#"{"number":7,"title":"Fix filters","body":"Body text","headRefName":"feature/filters","url":"https://example.test/pr/7"}"#;
    let github_cli = parse_metadata(&run_prompt_link(
        "github_pr",
        "7",
        "https://example.test/pr/7",
        github_json,
    ));
    let github_gui = SessionLinkMetadata::new(
        SessionLinkProvider::GitHub,
        SessionLinkKind::PullRequest,
        "7",
        "https://example.test/pr/7",
        "feature/filters",
    )
    .expect("valid GitHub metadata")
    .to_session_metadata();
    assert_eq!(github_cli, github_gui);
}
