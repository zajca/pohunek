//! A store rewrite keeps every line no loader accepts, through the public
//! daemon store boundary.
//!
//! The fixture is a schema-10 store migrated to the current schema. Two lines a
//! build cannot load are appended: a current-schema session record carrying a
//! desired state this build does not know, and a line that is not JSON.

// Rust guideline compliant 2026-10-26

use pohunek_daemon::store::{ProjectRecord, SchemaMigration, Store};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

/// A line that is not JSON.
const CORRUPT_LINE: &str = "{not json at all";

fn fixture_store(root: &std::path::Path) -> (Store, std::path::PathBuf) {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-10 store")
        .write_all(SCHEMA_10.as_bytes())
        .expect("write frozen schema-10 store");
    let store = Store::new(path.clone());
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));
    (store, path)
}

/// A current-schema session line whose desired state a newer daemon wrote.
fn newer_session_line(path: &std::path::Path) -> String {
    let stored = std::fs::read_to_string(path).expect("read current store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    line["session_id"] = "s-from-a-newer-daemon".into();
    line["desired_state"] = "suspended".into();
    serde_json::to_string(&line).expect("serialize rejected line")
}

fn store_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("read store")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn project() -> ProjectRecord {
    serde_json::from_value(serde_json::json!({
        "git_common_dir": "/repo/.git",
        "repo_root": "/repo",
        "source": "manual",
        "added_at": "2026-01-01T00:00:00Z",
        "last_used_at": "2026-01-01T00:00:01Z"
    }))
    .expect("project record")
}

#[test]
fn every_rewrite_keeps_the_lines_no_loader_accepts() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let rejected = [newer_session_line(&path), CORRUPT_LINE.to_owned()];
    let mut content = std::fs::read_to_string(&path).expect("read current store");
    for line in &rejected {
        content.push_str(line);
        content.push('\n');
    }
    std::fs::write(&path, content).expect("append the rejected lines");
    let session = store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session");
    assert_eq!(session.session_id, FIXTURE_SESSION);

    store.record_session(&session).expect("rewrite the session");
    let lines = store_lines(&path);
    assert_eq!(
        lines[lines.len() - 2..],
        rejected,
        "the rejected lines follow the parsed records unchanged and in order"
    );
    assert_eq!(lines.len(), 3, "one parsed session and both rejected lines");

    store
        .mutate_project(std::path::Path::new("/repo/.git"), |_| Some(project()))
        .expect("write a project");
    store
        .remove_session(FIXTURE_SESSION)
        .expect("remove the session");
    let lines = store_lines(&path);
    assert_eq!(lines[lines.len() - 2..], rejected);
    assert_eq!(lines.len(), 3, "one project and both rejected lines");
    assert!(store.load_sessions().expect("load sessions").is_empty());
}

#[test]
fn a_store_without_rejected_lines_gains_none() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let session = store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session");

    store.record_session(&session).expect("rewrite the session");
    assert_eq!(store_lines(&path).len(), 1);
}
