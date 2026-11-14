//! Schema-19 store migration through the public daemon store boundary, and the
//! recorded agent option choices of a session in the current schema.
//!
//! The schema-19 fixture is a profile session in the shape the schema-19 daemon
//! writes: it records no option choices. The migration leaves it unchanged, so
//! it loads with an empty selection, and a recorded selection survives a
//! rewrite of the record, on the session info and on its resume binding.

// Rust guideline compliant 2026-11-14

use std::collections::BTreeMap;

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_19: &str = include_str!("../src/store/fixtures/schema-19/metadata.jsonl");
const CURRENT_SCHEMA: &str = include_str!("../src/store/fixtures/schema-20/metadata.jsonl");

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

/// The selection the current-schema fixture records.
fn recorded() -> BTreeMap<String, String> {
    [("mode", "bold"), ("tier", "base")]
        .into_iter()
        .map(|(option, choice)| (option.to_owned(), choice.to_owned()))
        .collect()
}

#[test]
fn schema_19_sessions_migrate_without_inventing_option_choices() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_19);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-19 store must migrate");
    };
    assert_eq!((from, to, records), (19, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_19
    );

    let session = store
        .load_sessions()
        .expect("load migrated sessions")
        .remove(0);
    assert!(session.info.agent_options.is_empty());
    let binding = session.recovery.expect("the session keeps its binding");
    assert!(binding.agent_options.is_empty());
    let rewritten = std::fs::read_to_string(root.path().join("metadata.jsonl"))
        .expect("read the migrated store");
    assert!(
        !rewritten.contains("agent_options"),
        "an empty selection is not written"
    );
    assert!(rewritten.contains(&format!("\"schema_version\":{STORE_SCHEMA_VERSION}")));
}

#[test]
fn a_recorded_selection_loads_and_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), CURRENT_SCHEMA);

    let mut record = store.load_sessions().expect("load the session").remove(0);
    assert_eq!(record.info.agent_options, recorded());
    assert_eq!(
        record
            .recovery
            .as_ref()
            .expect("the session has a binding")
            .agent_options,
        recorded()
    );
    record.info.name = Some("renamed".to_owned());
    "2026-11-14T00:00:00Z".clone_into(&mut record.info.updated_at);
    store.record_session(&record).expect("rewrite the record");

    let reloaded = store.load_sessions().expect("reload the session").remove(0);
    assert_eq!(reloaded.info.agent_options, recorded());
    assert_eq!(
        reloaded
            .recovery
            .expect("the binding survives")
            .agent_options,
        recorded()
    );
}
