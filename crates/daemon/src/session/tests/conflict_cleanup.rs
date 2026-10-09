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

use super::{hermetic_shell, temp_dir, UnreadableCandidateHost, UNREADABLE_CANDIDATE_PID};
use crate::procwatch::{HostInspector, ProcessIdentity, ProcessInspector};
use crate::runtime::lifecycle::tests::{JobScript, ScriptedSupervisor};
use crate::runtime::WorkerLauncher;
use crate::session::{test_supervision, SessionRegistry, SessionRegistryConfig};
use crate::store::{migrate_at_startup, DesiredState, Store, TransactionKind};

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
    runtime_root: PathBuf,
    state_root: PathBuf,
    registry: SessionRegistry,
    supervisor: Arc<ScriptedSupervisor>,
    id: SessionId,
}

impl Fixture {
    async fn new(journal_runtime: &str) -> Self {
        Self::new_over(journal_runtime, Arc::new(HostInspector::new())).await
    }

    async fn new_over(
        journal_runtime: &str,
        inspector: Arc<dyn crate::procwatch::ProcessInspector>,
    ) -> Self {
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
        let registry = Self::registry(
            &store,
            &runtime,
            &state,
            Arc::clone(&supervisor) as Arc<dyn WorkerLauncher>,
            inspector,
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
            runtime_root: runtime,
            state_root: state,
            registry,
            supervisor,
            id: SessionId(SESSION.to_owned()),
        }
    }

    /// Builds the fixture's registry configuration over these paths for an
    /// extra registry standing for a fresh daemon.
    fn registry(
        store: &Path,
        runtime: &Path,
        state: &Path,
        launcher: Arc<dyn WorkerLauncher>,
        inspector: Arc<dyn crate::procwatch::ProcessInspector>,
    ) -> SessionRegistry {
        SessionRegistry::new_with_launcher_and_inspector(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                store_path: Some(store.to_owned()),
                worker_runtime_root: Some(runtime.to_owned()),
                worker_state_root: Some(state.to_owned()),
                supervision: Some(test_supervision(runtime, state)),
                ..SessionRegistryConfig::default()
            },
            launcher,
            inspector,
        )
    }

    /// A fresh registry over the same store, journals and supervisor roots,
    /// standing for a daemon restarted after the interruption it reconciles.
    fn restarted(&self, launcher: Arc<dyn WorkerLauncher>) -> SessionRegistry {
        Self::registry(
            &self.store,
            &self.runtime_root,
            &self.state_root,
            launcher,
            Arc::new(HostInspector::new()),
        )
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
    write_journal(state, runtime, pid, start, RuntimePhase::Live);
}

fn write_terminal_journal(state: &Path, runtime: &str) {
    let mut child = pohunek_test_support::process_env::command("true")
        .spawn()
        .expect("spawn short-lived worker stand-in");
    let pid = child.id();
    child.wait().expect("reap worker stand-in");
    let start = HostInspector::new()
        .identity(std::process::id())
        .expect("inspect the test process")
        .expect("the test process runs")
        .start_identity;
    write_journal(state, runtime, pid, start, RuntimePhase::Terminal);
}

fn write_live_journal(state: &Path, runtime: &str, stand_in: &LiveWorkerStandIn) {
    let identity = stand_in.identity();
    write_journal(
        state,
        runtime,
        identity.pid,
        identity.start_identity,
        RuntimePhase::Live,
    );
}

fn write_journal(
    state: &Path,
    runtime: &str,
    pid: super::Pid,
    start: super::StartIdentity,
    phase: RuntimePhase,
) {
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
    journal.phase = phase;
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

/// A live process standing in for the journaled worker of the fixture: its
/// exact identity goes into the journal, so the supervisor's job can name it
/// and a scripted retirement can end it. Killed and reaped on drop.
struct LiveWorkerStandIn(std::process::Child);

impl LiveWorkerStandIn {
    fn spawn() -> Self {
        let child = pohunek_test_support::process_env::command("cat")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn live worker stand-in");
        Self(child)
    }

    /// The process table identity of the stand-in, as the reports would
    /// observe it.
    fn identity(&self) -> ProcessIdentity {
        HostInspector::new()
            .identity(self.0.id())
            .expect("inspect live worker stand-in")
            .expect("live worker stand-in runs")
    }
}

impl Drop for LiveWorkerStandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

/// A real partial retirement: the scripted supervisor ends the live journaled
/// worker but fails the retire call, so the generation cannot be proven
/// retired through it. The removal keeps its durable Remove intent instead of
/// dying with a mere stop, and a restarted daemon finishes the removal from
/// that intent.
#[tokio::test]
async fn a_partial_retirement_keeps_the_remove_intent_for_reconciliation() {
    let fixture = Fixture::new(RUNTIME).await;
    let stand_in = LiveWorkerStandIn::spawn();
    let identity = stand_in.identity();
    write_live_journal(&fixture.state_root, RUNTIME, &stand_in);
    fixture.supervisor.script_job(
        service_id(),
        JobScript::Present {
            state: ServiceState::Running,
            process: Some(identity),
            definition: None,
        },
    );
    fixture
        .supervisor
        .script_retire_ends_then_fails(service_id(), identity.pid);

    let error = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect_err("the supervisor failed its retire call");
    assert_eq!(
        error.code,
        crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE,
        "{error:?}"
    );
    assert_eq!(
        fixture.supervisor.retired(),
        vec![service_id()],
        "only the recorded generation's job was retired"
    );
    let pending = Store::new(fixture.store.clone())
        .load_sessions()
        .expect("read durable sessions")
        .into_iter()
        .find(|record| record.session_id == SESSION)
        .expect("the Remove intent stays durable over a partial retirement");
    assert_eq!(pending.desired_state, DesiredState::Removed);
    let transaction = pending.transaction.expect("removal transaction");
    assert_eq!(transaction.kind, TransactionKind::Remove);

    let restarted = fixture.restarted(Arc::new(ScriptedSupervisor::scripted()));
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");
    assert_eq!(
        fixture.record_count(),
        0,
        "reconciliation finished the removal from its intent"
    );
    restarted
        .inspect(&fixture.id)
        .await
        .expect_err("the session is gone after reconciliation");
}

/// `session.remove_accepting_unconfirmed` finishes a journal-proven conflict
/// sweep that an unreadable-marker candidate blocks: that candidate's
/// identity stays live and it is reported to the caller.
#[tokio::test]
async fn an_accepting_cleanup_removes_a_conflict_past_an_unreadable_marker_candidate() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let fixture =
        Fixture::new_over(RUNTIME, Arc::clone(&inspector) as Arc<dyn ProcessInspector>).await;
    inspector.set_live_candidates(true);
    inspector.set_candidates(1);
    let candidate = ProcessIdentity {
        pid: UNREADABLE_CANDIDATE_PID,
        start_identity: super::UNREADABLE_CANDIDATE_LIVE_START,
    };

    let removed = fixture
        .registry
        .remove_with(&fixture.id, crate::session::UnconfirmedCleanup::Accept)
        .await
        .expect("the accepting cleanup completes the journal-proven removal");
    assert!(removed.removed);
    assert_eq!(fixture.record_count(), 0);
    fixture
        .registry
        .inspect(&fixture.id)
        .await
        .expect_err("removed");
    assert_eq!(
        inspector.is_running(candidate).ok(),
        Some(true),
        "the unresolved candidate stays live"
    );
    assert_eq!(
        removed.accepted_unconfirmed_processes,
        vec![protocol::UnconfirmedProcess {
            pid: UNREADABLE_CANDIDATE_PID,
            // The identity the sieve enumerated, not the scripted live read.
            start_identity: protocol::ProcessStartIdentity::new(1),
            command: Some(UnreadableCandidateHost::candidate_comm(0)),
        }],
        "the removal reports exactly the accepted unreadable candidate"
    );
}

/// Produces the state both interrupted-removal regressions replay: a
/// journal-proven conflict removal whose supervisor retired the live
/// journaled worker but failed its retire call, leaving the durable remove
/// intent behind. The stand-in still belongs to the caller's test and is
/// killed and reaped on drop.
async fn interrupted_conflict_removal() -> (Fixture, LiveWorkerStandIn) {
    let fixture = Fixture::new(RUNTIME).await;
    let stand_in = LiveWorkerStandIn::spawn();
    let identity = stand_in.identity();
    write_live_journal(&fixture.state_root, RUNTIME, &stand_in);
    fixture.supervisor.script_job(
        service_id(),
        JobScript::Present {
            state: ServiceState::Running,
            process: Some(identity),
            definition: None,
        },
    );
    fixture
        .supervisor
        .script_retire_ends_then_fails(service_id(), identity.pid);
    let error = fixture
        .registry
        .remove(&fixture.id)
        .await
        .expect_err("the supervisor failed its retire call");
    assert_eq!(
        error.code,
        crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE,
        "{error:?}"
    );
    (fixture, stand_in)
}

/// A running job script with `definition` (or none) under the fixture's
/// service ID.
fn job_script(definition: Option<DefinitionFacts>) -> JobScript {
    JobScript::Present {
        state: ServiceState::Running,
        process: None,
        definition,
    }
}

/// The job definition of the recorded worker generation, which the identity
/// proof must accept.
fn own_job_definition() -> DefinitionFacts {
    DefinitionFacts {
        executable: PathBuf::from(WORKER_EXECUTABLE),
        arguments: vec![
            "--session-id".to_owned(),
            SESSION.to_owned(),
            "--worker-generation".to_owned(),
            GENERATION.to_owned(),
        ],
    }
}

#[tokio::test]
async fn a_conflict_proven_remove_intent_reproves_before_retiring_a_replaced_job() {
    let (fixture, _stand_in) = interrupted_conflict_removal().await;

    // The ended worker journals a terminal record, and a foreign job takes
    // the recorded service ID before the next daemon starts.
    write_terminal_journal(&fixture.state_root, RUNTIME);
    let foreign = Arc::new(ScriptedSupervisor::scripted());
    foreign.script_job(
        service_id(),
        job_script(Some(DefinitionFacts {
            executable: PathBuf::from("/opt/other/pohunek-sessiond"),
            arguments: Vec::new(),
        })),
    );
    let restarted = fixture.restarted(Arc::clone(&foreign) as Arc<dyn WorkerLauncher>);
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");
    assert!(
        foreign.retired().is_empty(),
        "the foreign job under the service ID is not retired"
    );
    assert_eq!(
        fixture.record_count(),
        1,
        "a failed identity proof keeps the durable session"
    );
    let listed = restarted.inspect(&fixture.id).await.expect("listed");
    let runtime = listed.runtime.expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Conflict);
    assert_eq!(
        runtime.loss_reason.as_deref(),
        Some("runtime_identity_mismatch"),
        "the replay failed closed on the foreign job identity"
    );
    let pending = Store::new(fixture.store.clone())
        .load_sessions()
        .expect("read durable sessions")
        .into_iter()
        .find(|record| record.session_id == SESSION)
        .expect("durable session");
    assert_eq!(pending.desired_state, DesiredState::Removed);
    let transaction = pending.transaction.expect("removal intent");
    assert_eq!(transaction.kind, TransactionKind::Remove);
    assert_eq!(
        transaction.phase,
        super::super::conflict_stop::PROVEN_CONFLICT_REMOVAL_PHASE,
        "only the conflict-proven intent phase is re-proven"
    );

    // The recorded generation's own job is a safe neighbor: the proof
    // accepts it, so the removal finishes.
    let own = Arc::new(ScriptedSupervisor::scripted());
    own.script_job(service_id(), job_script(Some(own_job_definition())));
    let settled = fixture.restarted(Arc::clone(&own) as Arc<dyn WorkerLauncher>);
    Box::pin(settled.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert_eq!(fixture.record_count(), 0, "the removal finished");
    assert!(
        foreign.retired().is_empty(),
        "the restart never touches the foreign job either"
    );
    assert_eq!(own.retired(), vec![service_id()]);
    settled
        .inspect(&fixture.id)
        .await
        .expect_err("removed after the identity proof passed");
}
