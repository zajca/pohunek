//! Schema-7 store migration through the public daemon store boundary.

// Rust guideline compliant 2026-10-12

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_7: &str = include_str!("../src/store/fixtures/schema-7/metadata.jsonl");

#[test]
fn schema_7_session_migrates_without_inventing_a_background_snapshot() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-7 store")
        .write_all(SCHEMA_7.as_bytes())
        .expect("write frozen schema-7 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-7 store must migrate");
    };
    assert_eq!((from, to, records), (7, STORE_SCHEMA_VERSION, 3));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_7
    );

    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let lines: Vec<serde_json::Value> = stored
        .lines()
        .map(|line| serde_json::from_str(line).expect("stored JSON"))
        .collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert_eq!(line["schema_version"], STORE_SCHEMA_VERSION);
    }
    let session_line = lines
        .iter()
        .find(|line| line["kind"] == "session")
        .expect("migrated session line");
    assert!(
        session_line["info"].get("background").is_none(),
        "a schema 7 session holds no background snapshot"
    );

    let resumes = store.load_resume().expect("load migrated resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0].session_id, "s-fixture-721");
    assert_eq!(
        resumes[0].native_session_id.as_deref(),
        Some("fixture-conversation")
    );
    assert_eq!(resumes[0].project_id, None);

    let worktrees = store.load_worktrees().expect("load migrated worktree");
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].session_id, "s-fixture-721");
    assert_eq!(worktrees[0].project_id, None);

    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].info.background, None);
    assert_eq!(sessions[0].info.subagents.len(), 1);
    assert_eq!(
        sessions[0].info.native_session_id.as_deref(),
        Some("fixture-conversation")
    );
}
