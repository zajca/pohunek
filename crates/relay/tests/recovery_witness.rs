//! Exercises independent witness chaining and restore invalidation.

use ed25519_dalek::SigningKey;
use pohunek_relay::recovery::{DenyIncident, WitnessEvent, WitnessStore};
use std::{
    os::unix::fs::PermissionsExt as _,
    sync::{Arc, Barrier},
};
use tempfile::tempdir;

fn witness() -> (tempfile::TempDir, WitnessStore) {
    let directory = tempdir().expect("temporary witness directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("make temporary witness directory owner-private");
    let key = SigningKey::from_bytes(&[7_u8; 32]);
    let store = WitnessStore::open(directory.path(), key, "test-key".to_owned())
        .expect("open witness storage");
    (directory, store)
}

#[test]
fn append_only_history_and_restore_generation_advance() {
    let (_directory, store) = witness();
    let first = store.begin_run(None, "relay_test", 1).expect("begin run");
    let clean = store.end_run(&first).expect("clean shutdown");
    let restored = store
        .advance_restore(&clean, "a".repeat(64), "0".repeat(64))
        .expect("restore advance");

    assert_eq!(first.sequence, 1);
    assert_eq!(clean.sequence, 2);
    assert_eq!(restored.sequence, 3);
    assert_eq!(restored.recovery_generation, 2);
    assert!(restored.active_run);
}

#[test]
fn exact_clean_retry_succeeds_but_a_successor_rejects_the_old_checkpoint() {
    let (_directory, store) = witness();
    let first = store.begin_run(None, "relay_test", 1).expect("begin run");
    let second = store.end_run(&first).expect("end run");
    assert_eq!(
        store.end_run(&first).expect("retry lost clean response"),
        second
    );

    let _third = store
        .begin_run(Some(&second), "relay_test", 1)
        .expect("append successor");

    let error = store.end_run(&first).expect_err("stale checkpoint");
    assert!(matches!(
        error,
        pohunek_relay::recovery::RecoveryError::StaleWitness
    ));
    assert_eq!(
        store
            .latest()
            .expect("latest witness")
            .expect("witness")
            .sequence,
        3
    );
}

#[test]
fn missing_latch_cannot_publish_a_new_clean_child() {
    let (directory, store) = witness();
    let active = store.begin_run(None, "relay_test", 1).expect("begin run");
    std::fs::remove_file(directory.path().join("witness.active"))
        .expect("simulate a missing active latch");
    assert!(matches!(
        store.end_run(&active),
        Err(pohunek_relay::recovery::RecoveryError::StaleWitness)
    ));
    assert!(!directory
        .path()
        .join("witness.00000000000000000002.json")
        .exists());
}

#[test]
fn two_witness_store_instances_cannot_branch_a_clean_child() {
    let (directory, store) = witness();
    let peer = WitnessStore::open(
        directory.path(),
        SigningKey::from_bytes(&[7_u8; 32]),
        "test-key".to_owned(),
    )
    .expect("open peer witness storage");
    let active = store.begin_run(None, "relay_test", 1).expect("begin run");
    let barrier = Arc::new(Barrier::new(3));
    let first_store = &store;
    let second_store = &peer;
    let (first, second) = std::thread::scope(|scope| {
        let first_barrier = Arc::clone(&barrier);
        let first_active = active.clone();
        let first = scope.spawn(move || {
            first_barrier.wait();
            first_store.end_run(&first_active)
        });
        let second_barrier = Arc::clone(&barrier);
        let second_active = active.clone();
        let second = scope.spawn(move || {
            second_barrier.wait();
            second_store.end_run(&second_active)
        });
        barrier.wait();
        (
            first.join().expect("first witness thread"),
            second.join().expect("second witness thread"),
        )
    });
    let clean = match (first, second) {
        (Ok(first), Ok(second)) => {
            assert_eq!(first, second);
            first
        }
        (Ok(clean), Err(_)) | (Err(_), Ok(clean)) => clean,
        (Err(first), Err(second)) => panic!("both clean writers failed: {first:?}, {second:?}"),
    };
    assert_eq!(
        store
            .end_run(&active)
            .expect("retry after concurrent attempt"),
        clean
    );
    assert!(!directory
        .path()
        .join("witness.00000000000000000003.json")
        .exists());
}

#[test]
fn crash_after_history_sync_recovers_the_contiguous_newest_record() {
    let (directory, store) = witness();
    let first = store.begin_run(None, "relay_test", 1).expect("begin run");
    let second = store.end_run(&first).expect("end run");
    let first_history = directory.path().join("witness.00000000000000000001.json");
    let current = directory.path().join("witness.current");
    std::fs::copy(first_history, current)
        .expect("simulate crash before current pointer replacement");
    std::fs::set_permissions(
        directory.path().join("witness.current"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("restore private current mode");

    let recovered = store
        .latest()
        .expect("read history after crash")
        .expect("witness");
    assert_eq!(recovered.sequence, second.sequence);
}

#[test]
fn active_latch_overrides_a_torn_clean_checkpoint() {
    let (directory, store) = witness();
    let active = store.begin_run(None, "relay_test", 1).expect("begin run");
    let clean = store.end_run(&active).expect("clean checkpoint");
    std::fs::write(directory.path().join("witness.active"), b"active\n")
        .expect("simulate crash before latch removal");
    std::fs::set_permissions(
        directory.path().join("witness.active"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("make latch private");
    let recovered = store
        .latest()
        .expect("dirty latch is conservative")
        .expect("witness");
    assert_eq!(recovered.sequence, active.sequence);
    assert!(recovered.active_run);
    assert_ne!(recovered.sequence, clean.sequence);
}

#[test]
fn exact_clean_retry_repairs_a_resurrected_latch_without_a_new_sequence() {
    let (directory, store) = witness();
    let active = store.begin_run(None, "relay_test", 1).expect("begin run");
    let clean = store.end_run(&active).expect("publish clean checkpoint");
    std::fs::write(directory.path().join("witness.active"), b"active\n")
        .expect("simulate durable child followed by crash before unlink visibility");
    std::fs::set_permissions(
        directory.path().join("witness.active"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("make resurrected latch private");
    let retried = store
        .end_run(&active)
        .expect("repair exact clean publication");
    assert_eq!(retried, clean);
    assert!(!directory.path().join("witness.active").exists());
    assert!(!directory
        .path()
        .join("witness.00000000000000000003.json")
        .exists());
}

#[test]
fn altered_history_signature_fails_closed() {
    let (directory, store) = witness();
    let first = store.begin_run(None, "relay_test", 1).expect("begin run");
    let history = directory.path().join("witness.00000000000000000001.json");
    let replacement = serde_json::to_vec(&first).expect("serialize witness");
    let mut altered = replacement;
    let offset = altered
        .iter()
        .position(|byte| *byte == b'a')
        .expect("json content");
    altered[offset] = b'b';
    std::fs::write(&history, altered).expect("alter history");
    std::fs::set_permissions(history, std::fs::Permissions::from_mode(0o600))
        .expect("restore private history mode");

    assert!(store.latest().is_err());
}

#[test]
fn unsafe_file_permissions_and_symlinks_fail_closed() {
    let (directory, store) = witness();
    let _first = store.begin_run(None, "relay_test", 1).expect("begin run");
    let current = directory.path().join("witness.current");
    std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o644))
        .expect("make current world-readable");
    assert!(store.latest().is_err());

    std::fs::remove_file(&current).expect("remove unsafe current fixture");
    std::os::unix::fs::symlink(
        directory.path().join("witness.00000000000000000001.json"),
        &current,
    )
    .expect("create malicious current symlink");
    assert!(store.latest().is_err());
}

#[test]
fn dirty_latch_remains_set_after_deny_incident() {
    let (_directory, store) = witness();
    let current = store.begin_run(None, "relay_test", 1).expect("begin run");
    let deny = store
        .record_deny_incident(
            &current,
            DenyIncident::Principal {
                principal_id: uuid::Uuid::now_v7(),
            },
        )
        .expect("persist durable deny incident");
    assert!(deny.active_run);
    assert_eq!(deny.event, WitnessEvent::DenyIncident);
    assert_eq!(deny.manifest_digest, current.manifest_digest);
    assert!(
        store
            .latest()
            .expect("read witness")
            .expect("witness")
            .active_run
    );
}

#[test]
fn witness_identity_and_generation_are_immutable_across_transitions() {
    let (_directory, store) = witness();
    let first = store.begin_run(None, "relay_test", 2).expect("begin run");
    assert!(store.begin_run(Some(&first), "other_relay", 2).is_err());
    assert!(store.begin_run(Some(&first), "relay_test", 1).is_err());
}
