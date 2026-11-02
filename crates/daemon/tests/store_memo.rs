//! The store reads what is in the file and writes what a full rewrite writes.
//!
//! The scenarios drive the public daemon store boundary over a schema-10
//! fixture migrated to the current schema, so the records are the shape the
//! daemon persists.

// Rust guideline compliant 2026-10-31

use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use pohunek_daemon::store::{
    ProjectRecord, SchemaMigration, SessionRecord, Store, STORE_SCHEMA_VERSION,
};

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

/// The bytes the daemon built before the store snapshot (main `4c7d2e9b8`) wrote
/// after the first step of [`golden_steps`], with only the schema 12, 13 and 14
/// changes applied (`schema_version` 14; `refuse_while_blocked` from schema 12 in
/// the rewritten session's `input_rules`).
const GOLDEN_AFTER_SESSION: &str = include_str!("fixtures/store-memo/after-session.jsonl");

/// The same bytes after the second step of [`golden_steps`], with the same
/// schema 12, 13 and 14 changes.
const GOLDEN_AFTER_PROJECT: &str = include_str!("fixtures/store-memo/after-project.jsonl");

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

/// A line that is not JSON.
const CORRUPT_LINE: &str = "{not json at all";

fn create_store(root: &Path, content: &str) -> (Store, PathBuf) {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private store")
        .write_all(content.as_bytes())
        .expect("write the store");
    (Store::new(path.clone()), path)
}

/// The schema-10 fixture migrated to the current schema.
fn migrated_store(root: &Path) -> (Store, PathBuf) {
    let (store, path) = create_store(root, SCHEMA_10);
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));
    (store, path)
}

fn fixture_session(store: &Store) -> SessionRecord {
    store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session")
}

fn project(label: &str) -> ProjectRecord {
    serde_json::from_value(serde_json::json!({
        "git_common_dir": "/repo/.git",
        "repo_root": "/repo",
        "custom_name": label,
        "source": "manual",
        "added_at": "2026-01-01T00:00:00Z",
        "last_used_at": "2026-01-01T00:00:01Z"
    }))
    .expect("project record")
}

#[test]
fn an_edit_that_keeps_the_size_inode_and_mtime_is_seen() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = migrated_store(root.path());
    store
        .record_project(&project("zzlabel-AAAA"))
        .expect("record the project");
    assert_eq!(
        store.load_projects().expect("load projects")[0]
            .custom_name
            .as_deref(),
        Some("zzlabel-AAAA")
    );

    // An outside writer rewrites the bytes in place and restores the mtime.
    let original = std::fs::read(&path).expect("read the store");
    let modified = std::fs::metadata(&path)
        .expect("store metadata")
        .modified()
        .expect("store mtime");
    let edited: Vec<u8> = String::from_utf8(original.clone())
        .expect("utf-8 store")
        .replace("zzlabel-AAAA", "zzlabel-BBBB")
        .into_bytes();
    assert_eq!(edited.len(), original.len());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open the store in place");
    file.seek(SeekFrom::Start(0)).expect("seek");
    file.write_all(&edited).expect("edit in place");
    file.set_modified(modified).expect("restore the mtime");
    drop(file);

    assert_eq!(
        store.load_projects().expect("load projects")[0]
            .custom_name
            .as_deref(),
        Some("zzlabel-BBBB"),
        "the edit is read"
    );
    store
        .record_session(&fixture_session(&store))
        .expect("write a session");
    assert_eq!(
        store.load_projects().expect("load projects")[0]
            .custom_name
            .as_deref(),
        Some("zzlabel-BBBB"),
        "the next write keeps the edit"
    );
}

/// The migrated fixture store with an unknown field on the accepted line, a
/// project line, a line no loader accepts and a line that is not JSON.
fn golden_input(root: &Path) -> (Store, PathBuf) {
    let (store, path) = migrated_store(root);
    let stored = std::fs::read_to_string(&path).expect("read the migrated store");
    let mut accepted: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored line");
    accepted["unknown_future_field"] = serde_json::json!({ "nested": [1, 2, 3] });
    let mut rejected = accepted.clone();
    rejected["session_id"] = "s-from-a-newer-daemon".into();
    rejected["desired_state"] = "suspended".into();
    let mut project_line = serde_json::to_value(project("zzlabel-AAAA")).expect("project line");
    project_line["kind"] = "project".into();
    project_line["schema_version"] = STORE_SCHEMA_VERSION.into();
    let content = format!(
        "{}\n{}\n{}\n{CORRUPT_LINE}\n",
        serde_json::to_string(&project_line).expect("project line"),
        serde_json::to_string(&accepted).expect("accepted line"),
        serde_json::to_string(&rejected).expect("rejected line"),
    );
    std::fs::write(&path, content).expect("write the golden input");
    (store, path)
}

/// Two writes over [`golden_input`]: the session is rewritten with a runtime
/// whose instance id only the runtime record carries, which a decode mirrors
/// into the session info, then the project is replaced. Returns the store file
/// after each.
fn golden_steps(root: &Path) -> (String, String) {
    let (store, path) = golden_input(root);
    let mut session = fixture_session(&store);
    assert_eq!(session.session_id, FIXTURE_SESSION);
    session
        .info
        .runtime
        .as_mut()
        .expect("the fixture session has a runtime")
        .worker_instance_id = None;
    store.record_session(&session).expect("write the session");
    let after_session = std::fs::read_to_string(&path).expect("read the store");
    store
        .record_project(&project("zzlabel-BBBB"))
        .expect("replace the project");
    let after_project = std::fs::read_to_string(&path).expect("read the store");
    (after_session, after_project)
}

#[test]
fn writes_produce_the_bytes_of_the_daemon_before_the_snapshot() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (after_session, after_project) = golden_steps(root.path());

    assert_eq!(after_session, GOLDEN_AFTER_SESSION);
    // The session line of the second write is the cached one.
    assert_eq!(after_project, GOLDEN_AFTER_PROJECT);
}

#[test]
fn a_set_aside_line_is_not_written_back() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = golden_input(root.path());
    let session = fixture_session(&store);
    store.record_session(&session).expect("write the session");
    let hash = {
        use sha2::{Digest as _, Sha256};
        format!("{:x}", Sha256::digest(CORRUPT_LINE.as_bytes()))
    };

    store
        .set_aside_rejected(&hash, true)
        .expect("set the corrupt line aside");
    store.record_session(&session).expect("write the session");

    let stored = std::fs::read_to_string(&path).expect("read the store");
    assert!(!stored.contains(CORRUPT_LINE), "the line stays out");
    assert_eq!(
        stored.lines().count(),
        3,
        "the project, the session and the other line"
    );
    let quarantine = std::fs::read_to_string(root.path().join("metadata.rejected.jsonl"))
        .expect("read the quarantine file");
    assert!(quarantine.contains("not json at all"));
}

#[test]
fn another_handle_sees_the_writes_of_the_first() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = migrated_store(root.path());
    let other = Store::new(path);
    assert!(other.load_projects().expect("load projects").is_empty());

    store
        .record_project(&project("first"))
        .expect("record a project");
    assert_eq!(other.load_projects().expect("load projects").len(), 1);
    other
        .record_project(&project("second"))
        .expect("record through the other handle");
    assert_eq!(
        store.load_projects().expect("load projects")[0]
            .custom_name
            .as_deref(),
        Some("second")
    );
    store
        .record_session(&fixture_session(&store))
        .expect("write a session");
    assert_eq!(
        other.load_projects().expect("load projects")[0]
            .custom_name
            .as_deref(),
        Some("second")
    );
}
