//! Cleanup of a previous-release session quarantined over its launch identity.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pohunek_platform::supervisor::{
    DefinitionFacts, Error as SupervisorError, ServiceId, ServiceState, Supervisor,
};
use pohunek_session_worker::{Journal, JournalRecord, RuntimePhase, WorkerOrigin};
use protocol::{RuntimeGeneration, RuntimeState, SessionId, SessionRuntime};

use super::{hermetic_shell, temp_dir};
use crate::procwatch::{HostInspector, ProcessInspector};
use crate::runtime::lifecycle::tests::{JobScript, ScriptedSupervisor};
use crate::runtime::WorkerLauncher;
use crate::session::{test_supervision, SessionRegistry, SessionRegistryConfig};
use crate::store::{migrate_at_startup, Store};

/// Store serialized by the v0.33.0 daemon before it had `native_launch`.
const PREVIOUS_STORE: &str = include_str!("../../store/fixtures/v0.33.0/metadata.jsonl");
const SESSION: &str = "s-512";
const WORKER: &str = "w-fixture";
const GENERATION: &str = "abcd2345";
const RUNTIME: &str = "r-fixture";
const OTHER_RUNTIME: &str = "r-other-fixture";
const WORKER_EXECUTABLE: &str = "/opt/pohunek/libexec/pohunek/0.33.0/pohunek-sessiond";

struct Fixture {
    store: PathBuf,
    journal: PathBuf,
    registry: SessionRegistry,
    supervisor: Arc<ScriptedSupervisor>,
    id: SessionId,
}

impl Fixture {
    async fn new(journal_runtime: &str) -> Self {
        let root = temp_dir("previous-release-conflict-cleanup");
        let data = root.join("data");
        let state = root.join("state/workers");
        let runtime = root.join("runtime/workers");
        for directory in [&data, &state, &runtime] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .expect("create private fixture directory");
        }
        let store = data.join("metadata.jsonl");
        fs::write(&store, PREVIOUS_STORE.replace("s-fixture-session", SESSION))
            .expect("write previous-release fixture");
        fs::set_permissions(&store, fs::Permissions::from_mode(0o600))
            .expect("private metadata store");
        migrate_at_startup(&store).expect("migrate previous-release store");
        let db = Store::new(store.clone());
        let mut record = db
            .load_sessions()
            .expect("load migrated store")
            .into_iter()
            .find(|record| record.session_id == SESSION)
            .expect("fixture session");
        assert_eq!(
            record
                .recovery
                .as_ref()
                .and_then(crate::store::ResumeBinding::reference_kind),
            Some(crate::agent::SessionRefKind::Id),
            "the current migration restored the historical launch kind"
        );
        record.runtime.service_id = Some(service_id().to_string());
        record.runtime.generation = Some(GENERATION.to_owned());
        record.runtime.worker_instance_id = Some(RUNTIME.to_owned());
        record.runtime.executable = Some(PathBuf::from(WORKER_EXECUTABLE));
        record.info.runtime = Some(SessionRuntime {
            state: RuntimeState::Live,
            runtime_generation: RuntimeGeneration::new(1),
            worker_id: Some(WORKER.to_owned()),
            worker_instance_id: Some(RUNTIME.to_owned()),
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        });
        db.record_session(&record)
            .expect("record supervised generation");
        write_dead_journal(&state, journal_runtime);
        let journal = state.join(SESSION).join(format!("{WORKER}.json"));
        let supervisor = Arc::new(ScriptedSupervisor::scripted());
        let registry = SessionRegistry::new_with_launcher_and_inspector(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                store_path: Some(store.clone()),
                worker_runtime_root: Some(runtime.clone()),
                worker_state_root: Some(state.clone()),
                supervision: Some(test_supervision(&runtime, &state)),
                ..SessionRegistryConfig::default()
            },
            Arc::clone(&supervisor) as Arc<dyn WorkerLauncher>,
            Arc::new(HostInspector::new()),
        );
        assert!(
            registry
                .insert_unavailable_record(
                    record,
                    RuntimeState::Conflict,
                    "launch_identity_reference_kind_mismatch"
                )
                .await
        );
        Self {
            store,
            journal,
            registry,
            supervisor,
            id: SessionId(SESSION.to_owned()),
        }
    }

    fn record_count(&self) -> usize {
        Store::new(self.store.clone())
            .load_sessions()
            .expect("read durable sessions")
            .len()
    }
}

fn service_id() -> ServiceId {
    ServiceId::parse(format!("{SESSION}.{GENERATION}")).expect("valid worker service id")
}

fn write_dead_journal(state: &Path, runtime: &str) {
    let mut child = pohunek_test_support::process_env::command("true")
        .spawn()
        .expect("spawn short-lived worker stand-in");
    let pid = child.id();
    child.wait().expect("reap worker stand-in");
    // The worker is dead, but its journal needs a realistic start identity
    // to distinguish older unreadable host processes from its descendants.
    let start = HostInspector::new()
        .identity(std::process::id())
        .expect("inspect the test process")
        .expect("the test process runs")
        .start_identity;
    let mut journal = JournalRecord::bootstrap(
        SESSION.to_owned(),
        WORKER.to_owned(),
        WorkerOrigin {
            executable: PathBuf::from(WORKER_EXECUTABLE),
            version: "0.33.0".to_owned(),
            generation: GENERATION.to_owned(),
        },
        pid,
        start.get().to_string(),
        "boot-test".to_owned(),
        (1, 2),
        "2026-10-04T00:00:00Z".to_owned(),
    );
    journal.phase = RuntimePhase::Live;
    journal.worker_instance_id = Some(runtime.to_owned());
    let directory = state.join(SESSION);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .expect("create worker journal directory");
    Journal::new(directory.join(format!("{WORKER}.json")))
        .write(&journal)
        .expect("write worker journal");
}

#[tokio::test]
async fn a_proven_dead_conflict_without_a_supervisor_job_can_be_removed_directly() {
    let fixture = Fixture::new(RUNTIME).await;
    let listed = fixture.registry.inspect(&fixture.id).await.expect("listed");
    assert_eq!(
        listed.runtime.expect("runtime").state,
        RuntimeState::Conflict
    );
    assert_eq!(fixture.record_count(), 1);
    let job = fixture.supervisor.inspect(&service_id()).await;
    assert!(matches!(job, Err(SupervisorError::NotFound(_))));

    let removed = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect("the journal-proven dead generation can be removed");
    assert!(removed.removed);
    assert!(removed.stopped);
    assert_eq!(fixture.record_count(), 0);
    fixture.registry.inspect(&fixture.id).await.unwrap_err();
    let retired = fixture.supervisor.retired();
    assert_eq!(retired.len(), 1);
    assert!(retired.iter().all(|job| job == &service_id()));
}

#[tokio::test]
async fn a_proven_dead_conflict_can_be_stopped_then_removed() {
    let fixture = Fixture::new(RUNTIME).await;
    let stopped = fixture
        .registry
        .stop(&fixture.id)
        .await
        .expect("stop dead worker");
    assert!(stopped.stopped);
    assert_eq!(fixture.record_count(), 1);
    let removed = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect("remove stopped session");
    assert!(removed.removed);
    assert_eq!(fixture.record_count(), 0);
}

#[tokio::test]
async fn a_conflict_with_another_runtime_in_its_journal_stays_untouched() {
    let fixture = Fixture::new(OTHER_RUNTIME).await;
    let before = fs::read(&fixture.store).expect("read store before removal");
    let stop = fixture
        .registry
        .stop(&fixture.id)
        .await
        .expect_err("a foreign runtime must not be stopped");
    assert_eq!(stop.code, "runtime_identity_mismatch");
    let error = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect_err("a foreign runtime must not be retired");
    assert_eq!(error.code, "runtime_identity_mismatch");
    assert_eq!(
        fs::read(&fixture.store).expect("read store after refusal"),
        before
    );
    assert_eq!(fixture.record_count(), 1);
    assert!(fixture.supervisor.retired().is_empty());
}

#[tokio::test]
async fn a_conflict_without_its_worker_journal_stays_untouched() {
    let fixture = Fixture::new(RUNTIME).await;
    fs::remove_file(&fixture.journal).expect("remove worker journal");
    let before = fs::read(&fixture.store).expect("read store before removal");
    let error = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect_err("an unproven worker must not be retired");
    assert_eq!(error.code, "runtime_identity_mismatch");
    assert_eq!(
        fs::read(&fixture.store).expect("read store after refusal"),
        before
    );
    assert_eq!(fixture.record_count(), 1);
    assert!(fixture.supervisor.retired().is_empty());
}

#[tokio::test]
async fn a_conflict_with_a_foreign_supervisor_job_stays_untouched() {
    let fixture = Fixture::new(RUNTIME).await;
    fixture.supervisor.script_job(
        service_id(),
        JobScript::Present {
            state: ServiceState::Running,
            process: None,
            definition: Some(DefinitionFacts {
                executable: PathBuf::from("/opt/other/pohunek-sessiond"),
                arguments: Vec::new(),
            }),
        },
    );
    let before = fs::read(&fixture.store).expect("read store before removal");
    let error = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect_err("a foreign supervisor job must not be retired");
    assert_eq!(error.code, "runtime_identity_mismatch");
    assert_eq!(
        fs::read(&fixture.store).expect("read store after refusal"),
        before
    );
    assert_eq!(fixture.record_count(), 1);
    assert!(fixture.supervisor.retired().is_empty());
}

#[tokio::test]
async fn a_conflict_with_an_unreadable_worker_journal_stays_untouched() {
    let fixture = Fixture::new(RUNTIME).await;
    fs::write(&fixture.journal, b"not json").expect("corrupt worker journal");
    let before = fs::read(&fixture.store).expect("read store before removal");
    let error = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect_err("an unreadable worker must not be retired");
    assert_eq!(error.code, "runtime_supervision_ambiguous");
    assert_eq!(
        fs::read(&fixture.store).expect("read store after refusal"),
        before
    );
    assert_eq!(fixture.record_count(), 1);
    assert!(fixture.supervisor.retired().is_empty());
}
