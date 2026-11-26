//! A stored session whose two runtime identity copies disagree.
//!
//! A record keeps the runtime instance twice: in `runtime.runtime_id` and in
//! `info.runtime.worker_instance_id`. The store refuses a rewrite whose instance
//! differs from the stored one in the same generation, except that it rewrites
//! an ended record whose stored copies disagree, and the rewrite sets both
//! copies. The fixture is a schema-10 record of a failed session
//! whose runtime was lost, with the copies split.

// Rust guideline compliant 2026-11-26

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use std::sync::Arc;

use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_daemon::store::{SessionWriteOutcome, Store};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::worker_binary;
use protocol::{RuntimeGeneration, SessionId, SessionState};

mod support;

#[path = "../src/procwatch/readable_host.rs"]
mod readable_host;

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

/// Instance only the `runtime` copy of a split record names.
const OTHER_INSTANCE: &str = "runtime-other-copy";

/// How the fixture record is shaped before the store is migrated and opened.
struct Shape {
    /// Instance of the `runtime` copy; the `info` copy keeps the fixture's.
    runtime_copy: &'static str,
    /// Session state of the record.
    state: &'static str,
    /// Runtime state of both copies.
    runtime_state: &'static str,
}

const SPLIT_ENDED: Shape = Shape {
    runtime_copy: OTHER_INSTANCE,
    state: "failed",
    runtime_state: "lost",
};

fn store_with(root: &Path, shape: &Shape) -> Store {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-10 store")
        .write_all(SCHEMA_10.as_bytes())
        .expect("write frozen schema-10 store");
    let store = Store::new(path);
    store.migrate_to_current().expect("migrate previous schema");
    // The fixture line is edited on disk: a rewrite through the store would
    // be refused as a different instance.
    let stored = std::fs::read_to_string(store.path()).expect("read the store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    line["runtime"]["runtime_id"] = shape.runtime_copy.into();
    line["runtime"]["state"] = shape.runtime_state.into();
    line["info"]["runtime"]["state"] = shape.runtime_state.into();
    line["info"]["state"] = shape.state.into();
    std::fs::write(
        store.path(),
        format!(
            "{}\n",
            serde_json::to_string(&line).expect("serialize line")
        ),
    )
    .expect("rewrite the store");
    store
}

fn only_session(store: &Store) -> protocol::SessionInfo {
    store.load_sessions().expect("load sessions").remove(0).info
}

#[tokio::test]
async fn a_failed_session_with_split_runtime_identity_can_be_removed() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = store_with(root.path(), &SPLIT_ENDED);
    assert_eq!(only_session(&store).state, SessionState::Failed);

    let env = TestEnv::new().expect("create the test environment");
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: env.runtime_dir().to_path_buf(),
        state_home: env.state_home().to_path_buf(),
        data_home: env.data_home().to_path_buf(),
        config_home: env.config_home().to_path_buf(),
        cache_home: env.cache_home().to_path_buf(),
        home: env.home().to_path_buf(),
        daemon_socket: env.root().join("daemon.sock"),
    };
    let mut config = support::hermetic_registry_config();
    config.store_path = Some(root.path().join("metadata.jsonl"));
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    config.supervision = Some(
        worker_environment
            .supervision(worker_binary())
            .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
    );
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        SessionRegistryConfig { ..config },
        Arc::new(SubprocessWorkerLauncher::new()),
        Arc::new(readable_host::ReadableHost::new()),
    );
    Box::pin(registry.reconcile_workers())
        .await
        .expect("startup reconciliation");

    let removed = registry
        .remove(&SessionId(FIXTURE_SESSION.to_owned()))
        .await
        .expect("the failed session is removable");

    assert!(removed.removed);
    assert!(
        store.load_sessions().expect("load sessions").is_empty(),
        "the durable record is gone"
    );
}

#[test]
fn a_rewrite_of_a_split_ended_record_makes_the_copies_agree() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = store_with(root.path(), &SPLIT_ENDED);
    let mut record = store.load_sessions().expect("load sessions").remove(0);
    let info_copy = record
        .info
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_instance_id.clone())
        .expect("info copy");
    assert_ne!(info_copy, OTHER_INSTANCE);
    // The candidate is shaped as the registry builds it: both copies name the
    // in-memory instance.
    record.runtime.worker_instance_id = Some(info_copy.clone());

    assert_eq!(
        store.record_session(&record).expect("rewrite"),
        SessionWriteOutcome::Applied
    );

    let repaired = store.load_sessions().expect("load sessions").remove(0);
    assert_eq!(
        repaired.runtime.worker_instance_id.as_deref(),
        Some(info_copy.as_str())
    );
    assert_eq!(
        repaired
            .info
            .runtime
            .and_then(|runtime| runtime.worker_instance_id),
        Some(info_copy)
    );
}

#[test]
fn a_competing_instance_of_a_consistent_ended_record_is_still_refused() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = store_with(
        root.path(),
        &Shape {
            runtime_copy: "runtime-c5e6a58de789f9a2a1f3d35ce21ac216",
            ..SPLIT_ENDED
        },
    );
    let mut record = store.load_sessions().expect("load sessions").remove(0);
    record.runtime.worker_instance_id = Some(OTHER_INSTANCE.to_owned());
    record
        .info
        .runtime
        .as_mut()
        .expect("runtime")
        .worker_instance_id = Some(OTHER_INSTANCE.to_owned());

    assert_eq!(
        store.record_session(&record).expect("rewrite"),
        SessionWriteOutcome::StaleRuntime
    );
}

#[test]
fn a_live_record_with_split_identity_still_refuses_another_instance() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = store_with(
        root.path(),
        &Shape {
            state: "running",
            runtime_state: "live",
            ..SPLIT_ENDED
        },
    );
    let mut record = store.load_sessions().expect("load sessions").remove(0);
    record.runtime.worker_instance_id = Some("runtime-third".to_owned());
    record
        .info
        .runtime
        .as_mut()
        .expect("runtime")
        .worker_instance_id = Some("runtime-third".to_owned());

    assert_eq!(
        store.record_session(&record).expect("rewrite"),
        SessionWriteOutcome::StaleRuntime
    );
}

#[test]
fn a_lower_generation_is_still_refused_for_a_split_ended_record() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = store_with(root.path(), &SPLIT_ENDED);
    let mut record = store.load_sessions().expect("load sessions").remove(0);
    record
        .info
        .runtime
        .as_mut()
        .expect("runtime")
        .runtime_generation = RuntimeGeneration::new(0);

    assert_eq!(
        store.record_session(&record).expect("rewrite"),
        SessionWriteOutcome::StaleRuntime
    );
}
