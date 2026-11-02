//! Schema-10 store migration through the public daemon store boundary.
//!
//! The fixture is a schema-10 record of a session whose runtime was lost, in
//! the shape the schema-10 daemon writes, with neutral paths and identifiers.
//! A runtime written before schema 11 records no exit verdict.

// Rust guideline compliant 2026-10-21

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use protocol::{RuntimeExit, RuntimeState, SessionState};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

fn fixture_store(root: &std::path::Path) -> Store {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-10 store")
        .write_all(SCHEMA_10.as_bytes())
        .expect("write frozen schema-10 store");
    Store::new(path)
}

#[test]
fn schema_10_lost_session_migrates_without_inventing_an_exit() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-10 store must migrate");
    };
    assert_eq!((from, to, records), (10, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_10
    );

    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.session_id, FIXTURE_SESSION);
    assert_eq!(session.info.state, SessionState::Failed);
    let runtime = session.info.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Lost);
    assert_eq!(
        runtime.exit, None,
        "a schema 10 runtime records no exit verdict"
    );
}

#[test]
fn a_recorded_exit_round_trips_through_the_store() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path());
    store.migrate_to_current().expect("migrate previous schema");
    let mut record = store
        .load_sessions()
        .expect("load migrated session")
        .remove(0);
    let exit = RuntimeExit {
        cause: "oom_kill".to_owned(),
        signal: Some(15),
        code: None,
        oom_killer: None,
        memory_peak_bytes: None,
        ended_at: None,
    };
    record.info.runtime.as_mut().expect("runtime").exit = Some(exit.clone());
    store.record_session(&record).expect("record the exit");

    let reloaded = store.load_sessions().expect("reload the session");
    assert_eq!(
        reloaded[0]
            .info
            .runtime
            .as_ref()
            .map(|runtime| &runtime.exit),
        Some(&Some(exit))
    );
}
