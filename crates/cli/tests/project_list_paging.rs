//! `project.list` against a project set larger than one response.
//!
//! A real daemon is served from this test process. Projects are registered
//! through the public `project.add` method, with display names that make each
//! record large where a scenario needs a set beyond one control line. The set
//! is listed through the `pohunek` binary and through raw `project.list`
//! requests, so each outcome is what an operator or a client observes.

// Rust guideline compliant 2026-11-26

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use pohunek_client::Client;
use pohunek_test_support::tempdir;
use protocol::{
    method, ProjectAddParams, ProjectInfo, ProjectRenameParams, Request, Response,
    MAX_CONTROL_LINE_BYTES,
};
use serde_json::{json, Value};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// Bytes of the display name of each large project and how many there are:
/// together they need more than one control line, while a page of the
/// client's size is still one request.
const LARGE_NAME_BYTES: usize = 20_000;
const LARGE_PROJECTS: usize = 60;

const _: () = assert!(LARGE_PROJECTS * LARGE_NAME_BYTES > MAX_CONTROL_LINE_BYTES);

/// Projects of the small set that is read page by page.
const SMALL_PROJECTS: usize = 25;

/// Projects of one page in the paging scenario.
const PAGE: usize = 10;

/// Creates an empty git repository below `root`.
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

/// Registers `count` projects whose display names have `name_bytes` bytes and
/// returns them ordered by label, then id (the order of an unpaged listing).
async fn add_projects(harness: &Harness, count: usize, name_bytes: usize) -> Vec<ProjectInfo> {
    let root = tempdir().expect("create the repository root");
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    let mut projects = Vec::new();
    for index in 0..count {
        let repo = init_repo(root.path(), &format!("repo-{index:04}"));
        let prefix = format!("{index:04}-");
        let name = format!(
            "{prefix}{}",
            "n".repeat(name_bytes.saturating_sub(prefix.len()))
        );
        let added = client
            .call::<method::ProjectAdd>(ProjectAddParams {
                path: Some(repo),
                name: Some(name),
                base_branch: None,
            })
            .await
            .expect("register a project");
        projects.push(added);
    }
    projects.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
    projects
}

/// Sends one `project.list` request with `params` as raw JSON and returns the
/// response. The reader accepts a line of any length, so a daemon that ignores
/// the paging parameters is observed rather than refused as a framing error.
fn raw_list(harness: &Harness, params: Value) -> Response {
    let mut stream = UnixStream::connect(harness.socket()).expect("connect to the daemon");
    let request =
        Request::new("paging-scenario", method::PROJECT_LIST, params).expect("valid request");
    let mut line = serde_json::to_string(&request).expect("serialize request");
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("send the request");
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .expect("read the response line");
    serde_json::from_str(&reply).expect("parse the response")
}

/// The project ids of a successful `project.list` response.
fn page_ids(response: Response) -> Vec<String> {
    let result = response.into_result().expect("a project.list result");
    result
        .as_array()
        .expect("project records")
        .iter()
        .map(|project| project["id"].as_str().expect("project id").to_owned())
        .collect()
}

fn ids(projects: &[ProjectInfo]) -> Vec<String> {
    projects.iter().map(|project| project.id.clone()).collect()
}

#[tokio::test]
async fn a_project_set_larger_than_one_response_is_listed_completely_by_the_cli() {
    let harness = Harness::start_durable().await;
    let projects = add_projects(&harness, LARGE_PROJECTS, LARGE_NAME_BYTES).await;

    let (code, listed) = harness.json(&["project", "list"]).await;

    assert_eq!(code, 0, "{listed}");
    let listed_ids: Vec<String> = listed["ok"]
        .as_array()
        .expect("project records")
        .iter()
        .map(|project| project["id"].as_str().expect("project id").to_owned())
        .collect();
    assert_eq!(listed_ids, ids(&projects));

    harness.stop().await;
}

#[tokio::test]
async fn a_page_whose_projects_exceed_one_response_is_a_typed_error() {
    let harness = Harness::start_durable().await;
    add_projects(&harness, LARGE_PROJECTS, LARGE_NAME_BYTES).await;

    let error = raw_list(&harness, json!({ "limit": LARGE_PROJECTS }))
        .into_result()
        .expect_err("the whole set does not fit one response");

    assert_eq!(error.code, "project_list_too_large", "{error:?}");
    assert!(
        error
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("cursor")),
        "{error:?}"
    );

    harness.stop().await;
}

#[tokio::test]
async fn limit_and_cursor_walk_the_set_in_id_order() {
    let harness = Harness::start_durable().await;
    let projects = add_projects(&harness, SMALL_PROJECTS, 12).await;
    let mut all = ids(&projects);
    all.sort();

    let first = page_ids(raw_list(&harness, json!({ "limit": PAGE })));
    assert_eq!(first, all[..PAGE]);
    let second = page_ids(raw_list(
        &harness,
        json!({ "limit": PAGE, "cursor": first[PAGE - 1] }),
    ));
    assert_eq!(second, all[PAGE..2 * PAGE]);
    let third = page_ids(raw_list(
        &harness,
        json!({ "limit": PAGE, "cursor": second[PAGE - 1] }),
    ));
    assert_eq!(third, all[2 * PAGE..]);

    harness.stop().await;
}

#[tokio::test]
async fn a_project_renamed_between_pages_is_listed_once() {
    let harness = Harness::start_durable().await;
    let projects = add_projects(&harness, SMALL_PROJECTS, 12).await;
    let mut all = ids(&projects);
    all.sort();

    let first = page_ids(raw_list(&harness, json!({ "limit": PAGE })));
    // A project of the first page and one of the second change label across
    // the whole set's label order between the two requests.
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    for (reference, name) in [
        (&all[0], "zzzz-renamed-last"),
        (&all[PAGE], "0000-renamed-first"),
    ] {
        client
            .call::<method::ProjectRename>(ProjectRenameParams {
                reference: reference.clone(),
                name: name.to_owned(),
            })
            .await
            .expect("rename a project");
    }
    let rest = page_ids(raw_list(
        &harness,
        json!({ "limit": SMALL_PROJECTS, "cursor": first[PAGE - 1] }),
    ));

    assert_eq!([first, rest].concat(), all);

    harness.stop().await;
}

#[tokio::test]
async fn a_zero_limit_is_refused() {
    let harness = Harness::start_durable().await;

    let zero = raw_list(&harness, json!({ "limit": 0 }))
        .into_result()
        .expect_err("a zero limit is refused");

    assert_eq!(zero.code, "bad_request", "{zero:?}");

    harness.stop().await;
}
