//! `project.actions` against an action set at and above the daemon's count bound.
//!
//! A real daemon is served from this test process. A project's in-repo
//! `.pohunek/actions.toml` defines the actions: one more than the bound answers
//! the typed `project_actions_too_large` error, while exactly the bound lists
//! completely in one reply. Both outcomes are observed through raw
//! `project.actions` requests on the daemon's control socket and through the
//! `pohunek` binary.

// Rust guideline compliant 2026-11-27

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use pohunek_client::Client;
use pohunek_test_support::tempdir;
use protocol::{method, ProjectAddParams, Request, Response};
use serde_json::json;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// The daemon's hard count bound on the resolved action list, mirroring
/// `MAX_PROJECT_ACTIONS` in `crates/daemon/src/project/config.rs`.
const BOUND: usize = 1024;

/// Creates an empty git repository below `root` and returns its path.
fn init_repo(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(&path).expect("create the repository directory");
    let status = pohunek_test_support::git::command()
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&path)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init");
    path
}

/// Writes a `.pohunek` config into `repo` that defines one shared template and
/// `count` actions all using it.
fn write_actions(repo: &Path, count: usize) {
    let dir = repo.join(".pohunek");
    std::fs::create_dir_all(&dir).expect("create .pohunek");
    std::fs::write(
        dir.join("templates.toml"),
        "[template.t]\nagent = \"shell\"\nprompt = \"p\"\n",
    )
    .expect("write templates.toml");
    let mut actions = String::new();
    for index in 0..count {
        use std::fmt::Write as _;
        write!(
            actions,
            "[action.a{index:04}]\ntemplate = \"t\"\nprovider = \"none\"\n\n"
        )
        .expect("writing a String cannot fail");
    }
    std::fs::write(dir.join("actions.toml"), actions).expect("write actions.toml");
}

/// Registers `repo` as a project and returns its id.
async fn register_project(harness: &Harness, repo: &Path) -> String {
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    let added = client
        .call::<method::ProjectAdd>(ProjectAddParams {
            path: Some(repo.to_path_buf()),
            name: Some("actions-bound".to_owned()),
            base_branch: None,
        })
        .await
        .expect("register the project");
    added.id
}

/// Sends one raw `project.actions` request and returns the parsed response. The
/// reader accepts a line of any length, so a daemon that answers the whole set
/// is observed rather than refused as a framing error.
fn raw_actions(harness: &Harness, reference: &str) -> Response {
    let mut stream = UnixStream::connect(harness.socket()).expect("connect to the daemon");
    let request = Request::new(
        "actions-bound-scenario",
        method::PROJECT_ACTIONS,
        json!({ "reference": reference }),
    )
    .expect("valid request");
    let mut line = serde_json::to_string(&request).expect("serialize request");
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("send the request");
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .expect("read the response line");
    serde_json::from_str(&reply).expect("parse the response")
}

#[tokio::test]
async fn an_action_set_one_above_the_bound_is_a_typed_error() {
    let harness = Harness::start_durable().await;
    let root = tempdir().expect("create the repository root");
    let repo = init_repo(root.path(), "repo-over");
    write_actions(&repo, BOUND + 1);
    let reference = register_project(&harness, &repo).await;

    let error = raw_actions(&harness, &reference)
        .into_result()
        .expect_err("one action above the bound does not list");
    assert_eq!(error.code, "project_actions_too_large", "{error:?}");

    let (code, shown) = harness.json(&["project", "actions", &reference]).await;
    assert_ne!(code, 0, "{shown}");
    assert_eq!(shown["err"]["code"], "project_actions_too_large", "{shown}");

    harness.stop().await;
}

#[tokio::test]
async fn an_action_set_at_the_bound_lists_completely_in_one_reply() {
    let harness = Harness::start_durable().await;
    let root = tempdir().expect("create the repository root");
    let repo = init_repo(root.path(), "repo-at");
    write_actions(&repo, BOUND);
    let reference = register_project(&harness, &repo).await;

    let result = raw_actions(&harness, &reference)
        .into_result()
        .expect("the bound-sized set fits one control line");
    let actions = result["actions"].as_array().expect("the action list");
    assert_eq!(actions.len(), BOUND, "every action of the set is listed");

    let (code, shown) = harness.json(&["project", "actions", &reference]).await;
    assert_eq!(code, 0, "{shown}");
    assert_eq!(
        shown["ok"]["actions"].as_array().map_or(0, Vec::len),
        BOUND,
        "{shown}"
    );

    harness.stop().await;
}
