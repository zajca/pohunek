// Rust guideline compliant 2026-06-26

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pohunek_prompt::{render, Provider};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-prompt-{tag}-{}-{nanos}-{n}",
        std::process::id(),
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn python_render(
    template_path: &Path,
    provider: &str,
    item_id: &str,
    context_json: &str,
) -> String {
    let mut child = Command::new("python3")
        .args([
            "-",
            template_path.to_str().expect("utf8 template path"),
            provider,
            item_id,
            context_json,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn python renderer");
    child
        .stdin
        .as_mut()
        .expect("python stdin")
        .write_all(
            br#"
import json
import re
import sys

template_path, provider, item_id, raw_json = sys.argv[1:5]
try:
    data = json.loads(raw_json)
except json.JSONDecodeError as exc:
    raise SystemExit(f"provider returned invalid JSON: {exc}")

def pick(*names, required=False):
    for name in names:
        value = data.get(name)
        if isinstance(value, str) and value:
            return value
    if required:
        raise SystemExit(f"provider JSON missing required field: {'/'.join(names)}")
    return ""

if provider == "github_pr":
    context = {
        "provider": "github",
        "number": item_id,
        "id": item_id,
        "title": pick("title", required=True),
        "body": pick("body", "description"),
        "branch": pick("headRefName", "branch", "branchName", required=True),
        "url": pick("url"),
    }
elif provider == "linear_issue":
    context = {
        "provider": "linear",
        "id": pick("identifier", "id") or item_id,
        "number": pick("identifier", "id") or item_id,
        "title": pick("title", required=True),
        "body": pick("description", "body"),
        "branch": pick("branchName", "branch", required=True),
        "url": pick("url"),
    }
else:
    raise SystemExit(f"unknown provider: {provider}")

with open(template_path, encoding="utf-8") as handle:
    template = handle.read()

unknown = sorted(set(re.findall(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}", template)) - set(context))
if unknown:
    raise SystemExit(f"template references unknown variable(s): {', '.join(unknown)}")

rendered = re.sub(
    r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}",
    lambda match: context[match.group(1)],
    template,
)
sys.stdout.write(rendered)
"#,
        )
        .expect("write python renderer");
    let output = child.wait_with_output().expect("wait python renderer");
    assert!(
        output.status.success(),
        "python renderer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("python stdout utf8")
}

fn assert_matches_python(provider: Provider, provider_name: &str, item_id: &str, context: &str) {
    let dir = temp_dir(provider_name);
    let template_path = dir.join("prompt.tmpl");
    let template = "provider=${provider}\nid=${id}\nnumber=${number}\ntitle=${title}\nbody=${body}\nbranch=${branch}\nurl=${url}\n";
    fs::write(&template_path, template).expect("write template");

    let expected = python_render(&template_path, provider_name, item_id, context);
    let actual = render(template, provider, item_id, context).expect("render prompt");

    assert_eq!(actual, expected);
}

#[test]
fn renders_linear_issue_byte_identical_to_python() {
    assert_matches_python(
        Provider::LinearIssue,
        "linear_issue",
        "LIN-123",
        r#"{"identifier":"LIN-123","id":"opaque-id","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}"#,
    );
}

#[test]
fn renders_github_pr_byte_identical_to_python() {
    assert_matches_python(
        Provider::GitHubPr,
        "github_pr",
        "7",
        r#"{"title":"Fix filters","body":"Body text","headRefName":"feature/filters","url":"https://example.test/pr/7"}"#,
    );
}

#[test]
fn github_body_falls_back_to_description() {
    assert_matches_python(
        Provider::GitHubPr,
        "github_pr",
        "8",
        r#"{"title":"Fallback body","description":"Description text","branch":"feature/fallback","url":""}"#,
    );
}

#[test]
fn linear_id_falls_back_to_item_id() {
    assert_matches_python(
        Provider::LinearIssue,
        "linear_issue",
        "LIN-404",
        r#"{"title":"Missing id","body":"Body text","branch":"lin-404"}"#,
    );
}

#[test]
fn rejects_unknown_template_variables_in_sorted_order() {
    let err = render(
        "${z_var} ${title} ${a_var}",
        Provider::LinearIssue,
        "LIN-1",
        r#"{"title":"Title","branchName":"lin-1"}"#,
    )
    .expect_err("unknown variables reject");

    assert_eq!(
        err.to_string(),
        "template references unknown variable(s): a_var, z_var"
    );
}

#[test]
fn rejects_missing_required_fields_like_python() {
    let err = render(
        "${title}",
        Provider::GitHubPr,
        "7",
        r#"{"body":"Body","headRefName":"feature/x"}"#,
    )
    .expect_err("missing title rejects");

    assert_eq!(
        err.to_string(),
        "provider JSON missing required field: title"
    );
}

#[test]
fn provider_values_are_never_reexpanded() {
    for prefix in ["", "plain", "with spaces", "dash.dot_123"] {
        for suffix in ["", "tail", " more text", "_suffix-7"] {
            let title = format!("{prefix}${{body}}{suffix}");
            let context = serde_json::json!({
                "identifier": "LIN-1",
                "title": title,
                "description": "provider body",
                "branchName": "lin-1",
            })
            .to_string();

            let rendered = render(
                "Title: ${title}\n",
                Provider::LinearIssue,
                "LIN-1",
                &context,
            )
            .expect("render prompt");

            assert_eq!(rendered, format!("Title: {prefix}${{body}}{suffix}\n"));
        }
    }
}
