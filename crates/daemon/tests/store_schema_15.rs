//! Schema-14 store migration through the public daemon store boundary, and the
//! loss evidence of a runtime in the current schema.
//!
//! The schema-14 fixture is a lost Pi session in the shape the schema-14 daemon
//! writes, with neutral paths and identifiers. Its runtime records no loss
//! evidence, which the current schema reads as none. The schema-15 fixture holds
//! one lost runtime with a loss cause and one with the exit keys of the current
//! schema; both survive a rewrite of the store.

// Rust guideline compliant 2026-11-02

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_14: &str = include_str!("../src/store/fixtures/schema-14/metadata.jsonl");
const SCHEMA_15: &str = include_str!("../src/store/fixtures/schema-15/metadata.jsonl");

fn fixture_store(root: &std::path::Path, contents: &str) -> Store {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private store")
        .write_all(contents.as_bytes())
        .expect("write frozen store");
    Store::new(path)
}

#[test]
fn schema_14_runtimes_migrate_without_inventing_loss_evidence() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_14);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-14 store must migrate");
    };
    assert_eq!((from, to, records), (14, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_14
    );

    let sessions = store.load_sessions().expect("load migrated session");
    let runtime = sessions[0].info.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.loss_cause, None);
    assert_eq!(runtime.exit, None);
}

#[test]
fn recorded_loss_evidence_loads_and_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_15);

    let sessions = store.load_sessions().expect("load the sessions");
    assert_eq!(sessions.len(), 2);
    let runtime = |index: usize| sessions[index].info.runtime.clone().expect("runtime");
    assert_eq!(runtime(0).loss_cause.as_deref(), Some("user_manager_lost"));
    assert_eq!(runtime(0).exit, None);
    let exit = runtime(1).exit.expect("recorded exit");
    assert_eq!(exit.oom_killer.as_deref(), Some("systemd_oomd"));
    assert_eq!(exit.memory_peak_bytes, Some(26_300_000_000));
    assert_eq!(exit.ended_at.as_deref(), Some("2026-11-01T08:39:00Z"));
    assert_eq!(runtime(1).loss_cause, None);

    let mut record = sessions[0].clone();
    record.info.name = Some("renamed".to_owned());
    store.record_session(&record).expect("rewrite the record");
    let reloaded = store.load_sessions().expect("reload the sessions");
    let renamed = reloaded
        .iter()
        .find(|session| session.session_id == record.session_id)
        .expect("renamed session");
    assert_eq!(
        renamed
            .info
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.loss_cause.as_deref()),
        Some("user_manager_lost")
    );
}
