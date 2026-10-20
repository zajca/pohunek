//! Acknowledgement of a committed terminal outcome: the worker of a stopped or
//! finished session exits and the daemon retires the job of that generation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pohunek_platform::supervisor::ServiceId;
use protocol::{SessionId, SessionInfo, SessionState};

use super::{
    drop_settled, exit_settled, native_report_params, params, resumable_params, scripted_registry,
    stored_record, temp_agents_dir_with, temp_dir, wait_until, RestartableDaemon,
};
use crate::runtime::lifecycle::tests::{LogCapture, ScriptedSupervisor};
use crate::session::{RuntimeHandle, SessionRegistry, SessionRegistryConfig, ShellCommand};

/// Shell of the sessions that end on their own once they read a line.
const EXITS_ON_INPUT: &str = "read line; exit 3";
/// Interval of the supervision retry loop in the scenarios that need it.
const RETRY_INTERVAL: Duration = Duration::from_millis(20);

struct Rig {
    daemon: RestartableDaemon,
    registry: SessionRegistry,
    supervisor: Arc<ScriptedSupervisor>,
    created: SessionInfo,
    service_id: ServiceId,
}

impl Rig {
    async fn sleeping(tag: &str) -> Self {
        Self::with_shell(tag, None).await
    }

    async fn exiting_on_input(tag: &str) -> Self {
        Self::with_shell(
            tag,
            Some(ShellCommand::new("/bin/sh", ["-c", EXITS_ON_INPUT])),
        )
        .await
    }

    /// A session of a resumable agent that has reported its native id.
    async fn resumable(tag: &str) -> Self {
        let claude_home = temp_dir(&format!("{tag}-claude-home"));
        let transcripts = claude_home.join("projects/fixture");
        std::fs::create_dir_all(&transcripts).expect("create the Claude transcript store");
        std::fs::write(transcripts.join(format!("native-{tag}.jsonl")), "{}\n")
            .expect("write the Claude transcript");
        let agents_dir = temp_agents_dir_with(
            tag,
            "resumable",
            &format!(
                "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
                claude_home.display()
            ),
        );
        let rig = Self::build(tag, None, Some(agents_dir), resumable_params()).await;
        let recorded = rig
            .registry
            .report_as_root(
                native_report_params(
                    &rig.registry,
                    rig.created.id.clone(),
                    "claude".to_owned(),
                    format!("native-{tag}"),
                    None,
                )
                .await,
            )
            .await;
        assert!(recorded.recorded, "native id captured");
        rig
    }

    async fn with_shell(tag: &str, shell: Option<ShellCommand>) -> Self {
        Self::build(tag, shell, None, params()).await
    }

    async fn build(
        tag: &str,
        shell: Option<ShellCommand>,
        agents_dir: Option<PathBuf>,
        create: protocol::SessionNewParams,
    ) -> Self {
        let daemon = RestartableDaemon::new(tag);
        let config = SessionRegistryConfig {
            shell_command: shell.unwrap_or_else(|| daemon.config.shell_command.clone()),
            supervision_retry_initial: RETRY_INTERVAL,
            agents_dir,
            ..daemon.config.clone()
        };
        let (registry, supervisor) = scripted_registry(config);
        let created = registry.create(create).await.expect("create session");
        let service_id = ServiceId::parse(
            stored_record(&daemon.store_path, &created.id)
                .runtime
                .service_id
                .expect("the record names its job"),
        )
        .expect("service id");
        Self {
            daemon,
            registry,
            supervisor,
            created,
            service_id,
        }
    }

    fn journal(&self) -> serde_json::Value {
        let record = stored_record(&self.daemon.store_path, &self.created.id);
        let state_root = self
            .registry
            .inner
            .config
            .worker_state_root
            .clone()
            .expect("worker state root");
        let path: PathBuf = state_root.join(&self.created.id.0).join(format!(
            "{}.json",
            record.runtime.worker_id.expect("recorded worker")
        ));
        serde_json::from_slice(&std::fs::read(path).expect("read the worker journal"))
            .expect("parse the worker journal")
    }

    fn worker_socket(&self) -> PathBuf {
        let runtime_root = self
            .registry
            .inner
            .config
            .worker_runtime_root
            .clone()
            .expect("worker runtime root");
        crate::runtime::lifecycle::worker_socket(&runtime_root, &self.created.id.0)
            .expect("worker socket path")
    }

    /// Waits until the job was retired `times` times.
    async fn wait_retired(&self, times: usize) {
        wait_until("the job to be retired", || async {
            (self.supervisor.retired().len() >= times).then_some(())
        })
        .await;
    }

    async fn wait_worker_gone(&self) {
        wait_until("the worker socket to disappear", || async {
            (!self.worker_socket().exists()).then_some(())
        })
        .await;
    }
}

async fn assert_terminal_unavailable(registry: &SessionRegistry, id: &SessionId) {
    let screen = registry
        .screen(id)
        .await
        .expect_err("screen of a stopped session");
    assert_eq!(screen.code, "session_terminal_unavailable", "{screen:?}");
    let output = registry
        .output(
            &protocol::SessionOutputParams::new(id.clone(), None, None, 4096, None)
                .expect("output params"),
        )
        .await
        .expect_err("output of a stopped session");
    assert_eq!(output.code, "session_terminal_unavailable", "{output:?}");
}

/// A stopped session's worker exits and its job is retired; the terminal
/// session answers `session_terminal_unavailable` before and after.
#[tokio::test]
async fn stopped_session_worker_exits_and_job_is_retired() {
    let rig = Rig::sleeping("ack-stop").await;
    rig.registry
        .screen(&rig.created.id)
        .await
        .expect("a live session answers its screen");
    assert!(rig.supervisor.retired().is_empty());

    assert!(
        rig.registry
            .stop(&rig.created.id)
            .await
            .expect("stop")
            .stopped
    );
    assert_terminal_unavailable(&rig.registry, &rig.created.id).await;

    rig.wait_retired(1).await;
    rig.wait_worker_gone().await;
    assert_eq!(
        rig.supervisor.retired(),
        std::slice::from_ref(&rig.service_id)
    );
    assert_eq!(rig.journal()["terminal_acknowledged"], true);
    assert_terminal_unavailable(&rig.registry, &rig.created.id).await;
    let inspected = rig
        .registry
        .inspect(&rig.created.id)
        .await
        .expect("inspect");
    assert_eq!(inspected.state, SessionState::Stopped);
}

/// An agent that ends on its own is acknowledged through the exit watcher's
/// commit.
#[tokio::test]
async fn naturally_exited_worker_is_acknowledged_and_retired() {
    let rig = Rig::exiting_on_input("ack-natural").await;
    let mut events = rig.registry.subscribe();
    rig.registry
        .write_input_to_session(&rig.created.id, "go")
        .await
        .expect("the input ends the shell");
    exit_settled(&mut events, &rig.created.id).await;
    assert_eq!(
        rig.registry
            .inspect(&rig.created.id)
            .await
            .expect("inspect")
            .state,
        SessionState::Failed
    );

    rig.wait_retired(1).await;
    rig.wait_worker_gone().await;
    assert_eq!(
        rig.supervisor.retired(),
        std::slice::from_ref(&rig.service_id)
    );
    assert_eq!(rig.journal()["terminal_acknowledged"], true);
}

/// The exit watcher and a stop racing for the terminal commit acknowledge the
/// generation once: only the commit that applied sends.
#[tokio::test]
async fn terminal_race_acknowledges_once() {
    let rig = Rig::exiting_on_input("ack-race").await;
    let mut events = rig.registry.subscribe();
    let stopping = {
        let registry = rig.registry.clone();
        let id = rig.created.id.clone();
        tokio::spawn(async move { registry.stop(&id).await })
    };
    let _ = rig
        .registry
        .write_input_to_session(&rig.created.id, "go")
        .await;
    let _ = stopping.await.expect("the stop task joins");
    let id = rig.created.id.clone();
    let supervisor = Arc::clone(&rig.supervisor);
    let service_id = rig.service_id.clone();
    let store_path = rig.daemon.store_path.clone();
    drop_settled(rig.registry, &mut events, &id).await;

    assert_eq!(
        supervisor.retired(),
        [service_id],
        "one acknowledgement, one retirement"
    );
    assert!(stored_record(&store_path, &id).info.state.is_terminal());
}

/// A stop the worker refuses commits nothing, so nothing is acknowledged.
#[tokio::test]
async fn failed_stop_sends_no_acknowledgement() {
    let rig = Rig::sleeping("ack-failed-stop").await;
    let worker = {
        let sessions = rig.registry.inner.sessions.lock().await;
        let RuntimeHandle::Worker(worker) = &sessions[&rig.created.id].runtime else {
            panic!("live worker runtime");
        };
        worker.clone()
    };
    worker
        .release_controller()
        .await
        .expect("release the lease");

    rig.registry
        .stop(&rig.created.id)
        .await
        .expect_err("the worker refuses the stop");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(rig.supervisor.retired().is_empty());
    assert_eq!(rig.journal()["terminal_acknowledged"], false);
    assert!(rig.worker_socket().exists());
}

/// An acknowledgement the worker cannot take is logged once and changes
/// nothing else: the session stays terminal and the retention remains the
/// fallback.
#[tokio::test]
async fn failed_acknowledgement_falls_back_to_retention() {
    let capture = LogCapture::default();
    let _logs = capture.install();
    let rig = Rig::sleeping("ack-fails").await;
    let held = rig
        .registry
        .hold_next_terminal_acknowledgement(&rig.created.id);
    assert!(
        rig.registry
            .stop(&rig.created.id)
            .await
            .expect("stop")
            .stopped
    );
    held.entered.notified().await;
    // The controller lease is gone before the acknowledgement reaches the
    // worker.
    let worker = {
        let sessions = rig.registry.inner.sessions.lock().await;
        let RuntimeHandle::Worker(worker) = &sessions[&rig.created.id].runtime else {
            panic!("terminal entry keeps its worker handle");
        };
        worker.clone()
    };
    worker
        .release_controller()
        .await
        .expect("release the lease");
    held.release.notify_one();

    wait_until("the failed acknowledgement to be logged", || async {
        capture
            .text()
            .contains("terminal acknowledgement failed")
            .then_some(())
    })
    .await;
    assert!(
        rig.supervisor.retired().is_empty(),
        "no retirement follows the failed acknowledgement"
    );
    assert_eq!(rig.journal()["terminal_acknowledged"], false);
    assert_eq!(
        rig.registry
            .inspect(&rig.created.id)
            .await
            .expect("inspect")
            .state,
        SessionState::Stopped
    );
}

/// A retirement the supervisor refuses after the acknowledgement is retried
/// by the supervision loop.
#[tokio::test]
async fn acknowledged_generation_retirement_is_retried() {
    let rig = Rig::sleeping("ack-retire-retry").await;
    rig.supervisor
        .script_retire_unavailable(rig.service_id.clone());
    rig.registry.stop(&rig.created.id).await.expect("stop");

    rig.wait_retired(2).await;
    assert_eq!(
        rig.supervisor.retired(),
        [rig.service_id.clone(), rig.service_id.clone()]
    );
    assert_eq!(rig.journal()["terminal_acknowledged"], true);
}

/// A removal that wins the race against the parked acknowledgement retires
/// the job itself; the acknowledgement then sends nothing.
#[tokio::test]
async fn remove_racing_acknowledgement_skips_ack() {
    let capture = LogCapture::default();
    let _logs = capture.install();
    let rig = Rig::sleeping("ack-remove-race").await;
    let held = rig
        .registry
        .hold_next_terminal_acknowledgement(&rig.created.id);
    rig.registry.stop(&rig.created.id).await.expect("stop");
    held.entered.notified().await;

    rig.registry
        .remove(&rig.created.id)
        .await
        .expect("remove the stopped session");
    let retired_by_removal = rig.supervisor.retired();
    held.release.notify_one();
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert_eq!(retired_by_removal, std::slice::from_ref(&rig.service_id));
    assert_eq!(
        rig.supervisor.retired(),
        retired_by_removal,
        "the parked acknowledgement retires nothing"
    );
    assert!(!capture.text().contains("terminal acknowledgement failed"));
}

/// A removal of a live session owns its retirement: no acknowledgement task
/// runs for it.
#[tokio::test]
async fn remove_of_a_live_session_retires_without_acknowledging() {
    let rig = Rig::sleeping("ack-remove-live").await;
    let held = rig
        .registry
        .hold_next_terminal_acknowledgement(&rig.created.id);
    rig.registry
        .remove(&rig.created.id)
        .await
        .expect("remove the live session");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        rig.supervisor.retired(),
        std::slice::from_ref(&rig.service_id)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), held.entered.notified())
            .await
            .is_err(),
        "a removal spawns no acknowledgement"
    );
}

/// A resume that mints a new generation before the parked acknowledgement runs
/// leaves the new generation alone.
#[tokio::test]
async fn resume_before_acknowledgement_retires_only_old_generation() {
    let rig = Rig::resumable("ack-resume-race").await;
    let held = rig
        .registry
        .hold_next_terminal_acknowledgement(&rig.created.id);
    rig.registry.stop(&rig.created.id).await.expect("stop");
    held.entered.notified().await;

    rig.registry
        .resume(&rig.created.id)
        .await
        .expect("resume the stopped session");
    held.release.notify_one();
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let resumed = rig
        .registry
        .inspect(&rig.created.id)
        .await
        .expect("inspect");
    assert_eq!(resumed.state, SessionState::Running);
    assert_eq!(
        rig.supervisor.retired(),
        std::slice::from_ref(&rig.service_id),
        "only the old generation's job was retired"
    );
    rig.registry
        .screen(&rig.created.id)
        .await
        .expect("a live session answers its screen");
}

impl Rig {
    /// Ends this daemon without its background work and starts a replacement
    /// over the same stores and supervisor, as a restart does.
    async fn restart(
        self,
    ) -> (
        SessionRegistry,
        Arc<ScriptedSupervisor>,
        SessionInfo,
        ServiceId,
        RestartableDaemon,
    ) {
        let config = self.registry.inner.config.clone();
        self.registry.begin_daemon_shutdown();
        let inner = Arc::downgrade(&self.registry.inner);
        drop(self.registry);
        wait_until("the tasks of the first daemon to end", || {
            let ended = inner.upgrade().is_none();
            async move { ended.then_some(()) }
        })
        .await;
        let restarted = SessionRegistry::new_with_launcher_and_inspector(
            config,
            Arc::clone(&self.supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
            Arc::new(crate::procwatch::readable_host::ReadableHost::new()),
        );
        (
            restarted,
            self.supervisor,
            self.created,
            self.service_id,
            self.daemon,
        )
    }
}

/// A daemon that ended after the acknowledgement but before the retirement
/// finished: the restart retires the generation from its terminal journal.
#[tokio::test]
async fn acknowledged_worker_job_is_retired_after_restart() {
    let rig = Rig::sleeping("ack-restart-retire").await;
    rig.supervisor
        .script_retire_unavailable(rig.service_id.clone());
    rig.registry.stop(&rig.created.id).await.expect("stop");
    rig.wait_retired(1).await;
    rig.wait_worker_gone().await;

    let (restarted, supervisor, _created, service_id, _daemon) = rig.restart().await;
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    assert_eq!(supervisor.retired(), [service_id.clone(), service_id]);
}

/// A worker that kept serving its exited runtime because the daemon ended
/// before it acknowledged is acknowledged and retired by the next start.
#[tokio::test]
async fn retained_terminal_workers_are_acknowledged_on_startup() {
    let rig = Rig::sleeping("ack-startup").await;
    rig.registry.suppress_terminal_acknowledgement();
    rig.registry.stop(&rig.created.id).await.expect("stop");
    assert!(rig.worker_socket().exists(), "the worker is retained");
    assert!(rig.supervisor.retired().is_empty());
    let socket = rig.worker_socket();

    let (restarted, supervisor, created, service_id, _daemon) = rig.restart().await;
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    wait_until("the retained worker to exit", || async {
        (!socket.exists()).then_some(())
    })
    .await;
    wait_until("the job to be retired", || async {
        (supervisor.retired() == [service_id.clone()]).then_some(())
    })
    .await;
    let info = restarted.inspect(&created.id).await.expect("inspect");
    assert_eq!(info.state, SessionState::Stopped);
}

/// An acknowledgement whose answer is lost although it took effect (the worker
/// exits before it answers) still ends with the job retired: the supervision
/// retry finds the socket gone and the journal terminal.
#[tokio::test]
async fn lost_acknowledgement_reply_still_retires_the_job() {
    let rig = Rig::sleeping("ack-lost-reply").await;
    let held = rig
        .registry
        .hold_next_terminal_acknowledgement(&rig.created.id);
    rig.registry.stop(&rig.created.id).await.expect("stop");
    held.entered.notified().await;
    let worker = {
        let sessions = rig.registry.inner.sessions.lock().await;
        let RuntimeHandle::Worker(worker) = &sessions[&rig.created.id].runtime else {
            panic!("terminal entry keeps its worker handle");
        };
        worker.clone()
    };
    worker
        .acknowledge_terminal()
        .await
        .expect("the acknowledgement takes effect");
    rig.wait_worker_gone().await;
    assert!(rig.supervisor.retired().is_empty());

    // The parked task sends its acknowledgement after the worker is gone.
    rig.registry.fail_next_terminal_acknowledgement();
    held.release.notify_one();

    rig.wait_retired(1).await;
    assert_eq!(
        rig.supervisor.retired(),
        std::slice::from_ref(&rig.service_id)
    );
}
