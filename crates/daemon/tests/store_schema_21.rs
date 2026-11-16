//! Schema-20 store migration through the public daemon store boundary, and the
//! recorded in-repo hook switch of a session in the current schema.
//!
//! The schema-20 fixtures are a session and a worktree binding in the shape the
//! schema-20 daemon writes: no session of that schema could skip the in-repo
//! hook layer. The migration leaves them unchanged, so they load with the
//! layer running, and a recorded switch survives a rewrite of the session
//! record, its resume binding and its worktree binding.

// Rust guideline compliant 2026-11-15

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_20_SESSION: &str = include_str!("../src/store/fixtures/schema-20/metadata.jsonl");
const SCHEMA_20_WORKTREE: &str = include_str!("../src/store/fixtures/schema-20/worktree.jsonl");
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
fn schema_20_records_migrate_with_the_repository_hooks_running() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let frozen = format!("{SCHEMA_20_SESSION}{SCHEMA_20_WORKTREE}");
    let store = fixture_store(root.path(), &frozen);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-20 store must migrate");
    };
    assert_eq!((from, to, records), (20, STORE_SCHEMA_VERSION, 2));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        frozen
    );

    let session = store
        .load_sessions()
        .expect("load migrated sessions")
        .remove(0);
    assert!(!session.info.skip_repo_hooks);
    let binding = session.recovery.expect("the session keeps its binding");
    assert!(!binding.skip_repo_hooks);
    let worktree = store
        .load_worktrees()
        .expect("load migrated worktrees")
        .remove(0);
    assert!(!worktree.skip_repo_hooks);
    let rewritten = std::fs::read_to_string(root.path().join("metadata.jsonl"))
        .expect("read the migrated store");
    assert!(
        !rewritten.contains("skip_repo_hooks"),
        "a running layer is not written"
    );
    assert_eq!(
        rewritten
            .matches(&format!("\"schema_version\":{STORE_SCHEMA_VERSION}"))
            .count(),
        2
    );
}

#[test]
fn a_recorded_switch_loads_and_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), CURRENT_SCHEMA);

    let mut record = store.load_sessions().expect("load the session").remove(0);
    assert!(record.info.skip_repo_hooks);
    assert!(
        record
            .recovery
            .as_ref()
            .expect("the session has a binding")
            .skip_repo_hooks
    );
    record.info.name = Some("renamed".to_owned());
    "2026-11-15T00:00:00Z".clone_into(&mut record.info.updated_at);
    store.record_session(&record).expect("rewrite the record");
    let mut worktree = store.load_worktrees().expect("load the worktree").remove(0);
    assert!(worktree.skip_repo_hooks);
    "2026-11-15T00:00:00Z".clone_into(&mut worktree.updated_at);
    store
        .record_worktree(&worktree)
        .expect("rewrite the worktree binding");

    let reloaded = store.load_sessions().expect("reload the session").remove(0);
    assert!(reloaded.info.skip_repo_hooks);
    assert!(
        reloaded
            .recovery
            .expect("the binding survives")
            .skip_repo_hooks
    );
    let worktree = store
        .load_worktrees()
        .expect("reload the worktree")
        .remove(0);
    assert!(worktree.skip_repo_hooks);
}
