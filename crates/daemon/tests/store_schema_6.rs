//! Schema-5 store migration through the public daemon store boundary.

// Rust guideline compliant 2026-10-12

use pohunek_daemon::agent::SessionRef;
use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

/// A schema-5 resume binding of a Pi package session: its frozen spec assigns
/// the launch reference.
const SCHEMA_5: &str = include_str!("../src/store/fixtures/schema-5/metadata.jsonl");

/// A schema-6 resume binding whose assigned reference also covers forks.
const SCHEMA_6: &str = include_str!("../src/store/fixtures/schema-6/metadata.jsonl");

/// A schema-16 session line whose recovery snapshot and capabilities still
/// carry fork data.
const SCHEMA_16_SESSION_WITH_FORK_DATA: &str = concat!(
    r#"{"kind":"session","schema_version":16,"session_id":"s-fork-data","#,
    r#""info":{"capabilities":{"resume":true,"fork":true}},"#,
    r#""recovery":{"native_launch":{"fork_args":["--fork","{reference}"],"#,
    r#""assigned":{"assign_fork":true}}}}"#,
    "\n"
);

#[test]
fn a_schema_5_assigned_binding_migrates_and_still_resumes_with_its_assigned_launch() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-5 store")
        .write_all(SCHEMA_5.as_bytes())
        .expect("write frozen schema-5 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from, to, records, ..
    } = migrated
    else {
        panic!("schema-5 store must migrate");
    };
    assert_eq!((from, to, records), (5, STORE_SCHEMA_VERSION, 1));
    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let line: serde_json::Value = serde_json::from_str(stored.trim()).expect("stored JSON");
    assert_eq!(line["schema_version"], STORE_SCHEMA_VERSION);
    assert!(
        line["native_launch"]["assigned"].is_object(),
        "the migrated launch keeps its assigned reference"
    );

    let bindings = store.load_resume().expect("load migrated binding");
    assert_eq!(bindings.len(), 1);
    let launch = bindings[0]
        .native_launch
        .as_ref()
        .expect("frozen native launch");
    assert!(
        launch.assigned().is_some(),
        "the launch reference is assigned"
    );
    let source = SessionRef::id("0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee").expect("reference");
    let resume = launch
        .resume_argv(&source)
        .expect("the session still resumes");
    assert_eq!(resume, ["--session", source.value()]);
}

fn migrated_text(contents: &str) -> String {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private store")
        .write_all(contents.as_bytes())
        .expect("write frozen store");
    Store::new(path.clone())
        .migrate_to_current()
        .expect("migrate previous schema");
    std::fs::read_to_string(&path).expect("read migrated store")
}

#[test]
fn migration_drops_stored_fork_data_instead_of_keeping_it() {
    assert!(
        SCHEMA_5.contains("fork_args"),
        "the frozen line carries fork data"
    );
    assert!(
        SCHEMA_6.contains("assign_fork"),
        "the frozen line carries fork data"
    );
    for contents in [SCHEMA_5, SCHEMA_6, SCHEMA_16_SESSION_WITH_FORK_DATA] {
        let stored = migrated_text(contents);
        assert!(!stored.contains("fork_args"), "{stored}");
        assert!(!stored.contains("assign_fork"), "{stored}");
    }

    let session: serde_json::Value =
        serde_json::from_str(migrated_text(SCHEMA_16_SESSION_WITH_FORK_DATA).trim())
            .expect("stored JSON");
    assert_eq!(session["info"]["capabilities"]["fork"], false, "{session}");
    assert_eq!(session["info"]["capabilities"]["resume"], true, "{session}");
}
