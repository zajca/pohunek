//! Schema-17 store migration through the public daemon store boundary, and the
//! context package pin of a session in the current schema.
//!
//! The schema-17 fixture is a lost Pi session in the shape the schema-17 daemon
//! writes: its binding records no context package pin, which the current schema
//! reads as none and never backfills from the package installed now. The
//! current-schema fixture records the digest of the context package the session
//! reads, and the digest survives a rewrite of the store and is reported as a
//! pinned package by the retention scan.

// Rust guideline compliant 2026-11-11

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use protocol::PackageDigest;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_17: &str = include_str!("../src/store/fixtures/schema-17/metadata.jsonl");
const SCHEMA_18: &str = include_str!("../src/store/fixtures/schema-18/metadata.jsonl");

/// The digest the current-schema fixture records as its context pin.
const CONTEXT_PIN: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The digest the fixtures record as their runtime package pin.
const RUNTIME_PIN: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

fn digest(text: &str) -> PackageDigest {
    PackageDigest::parse(text).expect("fixture digest")
}

#[test]
fn schema_17_sessions_migrate_without_inventing_a_context_pin() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_17);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-17 store must migrate");
    };
    assert_eq!((from, to, records), (17, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_17
    );

    for session in store.load_sessions().expect("load migrated sessions") {
        let recovery = session.recovery.expect("a recovery binding");
        assert_eq!(recovery.context_pin, None);
    }
    let pinned = store.pinned_package_digests().expect("scan the pins");
    assert_eq!(
        pinned.digests.into_iter().collect::<Vec<_>>(),
        vec![digest(RUNTIME_PIN)],
        "only the runtime package is pinned"
    );
}

#[test]
fn a_recorded_context_pin_loads_survives_a_rewrite_and_is_retained() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_18);
    let expected = Some(digest(CONTEXT_PIN));

    let mut record = store.load_sessions().expect("load the session").remove(0);
    assert_eq!(
        record.recovery.as_ref().expect("recovery").context_pin,
        expected
    );
    record.info.name = Some("renamed".to_owned());
    store.record_session(&record).expect("rewrite the record");
    let reloaded = store.load_sessions().expect("reload the session");
    assert_eq!(
        reloaded[0].recovery.as_ref().expect("recovery").context_pin,
        expected
    );

    let pinned = store.pinned_package_digests().expect("scan the pins");
    assert!(
        pinned.digests.contains(&digest(CONTEXT_PIN)),
        "the context package is retained"
    );
    assert!(
        pinned.digests.contains(&digest(RUNTIME_PIN)),
        "the runtime package stays retained"
    );
}
