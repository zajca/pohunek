//! Schema-8 store migration through the public daemon store boundary.
//!
//! The fixture is the store the schema-8 daemon wrote for a shell session it
//! started and stopped, with the scenario's working directory, the worker
//! executable and the recorded shell replaced by neutral paths.

// Rust guideline compliant 2026-10-13

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use protocol::{RuntimeState, SessionState};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_8: &str = include_str!("../src/store/fixtures/schema-8/metadata.jsonl");

/// Session the schema-8 daemon recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

#[test]
fn schema_8_session_migrates_without_inventing_a_forced_output_close() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-8 store")
        .write_all(SCHEMA_8.as_bytes())
        .expect("write frozen schema-8 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-8 store must migrate");
    };
    assert_eq!((from, to, records), (8, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_8
    );

    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let lines: Vec<serde_json::Value> = stored
        .lines()
        .map(|line| serde_json::from_str(line).expect("stored JSON"))
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["schema_version"], STORE_SCHEMA_VERSION);
    assert!(
        lines[0]["info"].get("output_force_closed").is_none(),
        "a schema 8 session records no forced output close"
    );

    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.session_id, FIXTURE_SESSION);
    assert_eq!(session.info.state, SessionState::Stopped);
    assert_eq!(
        session.info.runtime.as_ref().map(|runtime| runtime.state),
        Some(RuntimeState::Terminal)
    );
    assert!(!session.info.output_force_closed);
}
