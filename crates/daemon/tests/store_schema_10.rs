//! Schema-9 store migration through the public daemon store boundary.
//!
//! The fixture is a schema-9 record of a live session of the Claude package
//! whose active agent is named like its runtime, in the shape the schema-9
//! daemon writes, with neutral paths and identifiers.

// Rust guideline compliant 2026-10-14

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use protocol::SessionState;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_9: &str = include_str!("../src/store/fixtures/schema-9/metadata.jsonl");

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

#[test]
fn schema_9_session_migrates_without_inventing_an_active_agent_origin() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-9 store")
        .write_all(SCHEMA_9.as_bytes())
        .expect("write frozen schema-9 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-9 store must migrate");
    };
    assert_eq!((from, to, records), (9, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_9
    );

    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let lines: Vec<serde_json::Value> = stored
        .lines()
        .map(|line| serde_json::from_str(line).expect("stored JSON"))
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["schema_version"], STORE_SCHEMA_VERSION);
    assert!(
        lines[0].get("active_agent_origin").is_none(),
        "a schema 9 record names no active agent origin"
    );

    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.session_id, FIXTURE_SESSION);
    assert_eq!(session.info.state, SessionState::Running);
    assert_eq!(session.info.active_agent.as_deref(), Some("claude"));
    assert_eq!(
        session.active_agent_origin, None,
        "the origin of a schema 9 active agent stays inferred at reconciliation"
    );
}
