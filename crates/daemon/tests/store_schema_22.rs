//! Schema-21 store migration through the public daemon store boundary, and the
//! recorded launch rights of a session in the current schema.
//!
//! The schema-21 fixture is a session in the shape the schema-21 daemon writes:
//! it records no launch rights. The migration leaves it unchanged, so it loads
//! with no launch rights and is not given any by a rewrite; the daemon reads
//! the profile file for such a session at its first use. A session record that
//! carries launch rights loads with them and keeps them through a rewrite.

// Rust guideline compliant 2026-11-16

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_21: &str = include_str!("../src/store/fixtures/schema-21/metadata.jsonl");
const CURRENT_SCHEMA: &str = include_str!("../src/store/fixtures/schema-22/metadata.jsonl");

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
fn a_session_recorded_before_launch_rights_loads_without_them() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_21);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-21 store must migrate");
    };
    assert_eq!((from, to, records), (21, STORE_SCHEMA_VERSION, 2));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_21
    );

    let mut session = store
        .load_sessions()
        .expect("load migrated sessions")
        .remove(0);
    assert!(session.launch_rights.is_none());
    "2026-11-16T00:00:00Z".clone_into(&mut session.info.updated_at);
    store.record_session(&session).expect("rewrite the record");
    let rewritten = std::fs::read_to_string(root.path().join("metadata.jsonl"))
        .expect("read the migrated store");
    assert!(
        !rewritten.contains("launch_rights"),
        "a rewrite never invents launch rights"
    );
}

#[test]
fn recorded_launch_rights_load_and_survive_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), CURRENT_SCHEMA);

    let mut session = store.load_sessions().expect("load the session").remove(0);
    let rights = session.launch_rights.clone().expect("recorded rights");
    assert!(rights.owner_equivalent);
    assert_eq!(rights.spawn_profiles, Some(vec!["worker".to_owned()]));
    "2026-11-16T00:00:00Z".clone_into(&mut session.info.updated_at);
    store.record_session(&session).expect("rewrite the record");

    let reloaded = store.load_sessions().expect("reload the session").remove(0);
    assert_eq!(reloaded.launch_rights, Some(rights));
}
