//! A reported native reference supersedes an assigned one, over real session
//! workers and a Pi-shaped runtime whose agent reports its conversation the way
//! a hooked agent does when the user switches conversations (`/clear`).

// Rust guideline compliant 2026-10-05

use super::*;
use crate::agent::NativeReferenceProvenance;

/// The reference the launch hands to the agent in its argv, as the fixture
/// agent sees it.
const ASSIGNED_FLAG: &str = "--session-id";

/// The runtime the fixture agent is registered as.
const RUNTIME: &str = "pi";

/// Rewrites the real Claude state adapter to report as the fixture runtime, so
/// the identity protocol exercised is the shipped one.
fn pi_adapter(dir: &std::path::Path) -> PathBuf {
    let source = pohunek_test_support::manifest_dir()
        .join("src/integration/assets/claude/pohunek-agent-state.sh");
    let script = fs::read_to_string(&source).expect("read the Claude state adapter");
    let adapted = script.replacen("agent = \"claude\"\n", "agent = \"pi\"\n", 1);
    assert_ne!(
        adapted, script,
        "the adapter names its provider on one line"
    );
    let path = dir.join("pi-agent-state.sh");
    write_executable(&path, &adapted);
    path
}

/// A fixture agent session under test: the registry, its worker, the durable
/// store and the FIFO the test drives the agent through.
///
/// The agent runs a command loop. `report <id>` sends the real adapter's
/// session hook for conversation `<id>` and acknowledges it in `acks` once the
/// worker answered; `nested` starts a child process and records its pid;
/// `exit` ends the agent. A start without `--session-id` (resume, fork) only
/// stays alive.
struct Rig {
    marker: PathBuf,
    acks: PathBuf,
    nested: PathBuf,
    python: PathBuf,
    script: PathBuf,
    commands: fs::File,
    store_path: PathBuf,
    config: SessionRegistryConfig,
    launcher: Arc<dyn crate::runtime::WorkerLauncher>,
    existence: &'static str,
    registry: SessionRegistry,
    session: SessionInfo,
    assigned: String,
}

impl Rig {
    async fn new(tag: &str, existence: &'static str) -> Self {
        Self::with_agents(tag, existence, None, RUNTIME).await
    }

    async fn with_agents(
        tag: &str,
        existence: &'static str,
        agents_dir: Option<PathBuf>,
        agent: &str,
    ) -> Self {
        let dir = temp_dir(tag);
        let marker = dir.join("argv.txt");
        let acks = dir.join("acks.txt");
        let nested = dir.join("nested.pid");
        let python = dir.join("python.path");
        let commands_path = dir.join("commands.fifo");
        let commands = hook_gate(&commands_path);
        let adapter = pi_adapter(&dir);
        // The worker accepts a launch claim only from a process whose
        // executable is named for the provider, so the agent runs on a copy of
        // the shell carrying that name.
        let interpreter = dir.join("pi-sh");
        fs::copy("/bin/sh", &interpreter).expect("copy the shell");
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700))
            .expect("make the interpreter executable");
        let script = dir.join("pi-switching");
        write_executable(
            &script,
            &format!(
                "#!{interpreter}\n\
                 printf 'launch\\n' >> '{marker}'\n\
                 printf '%s\\n' \"$@\" >> '{marker}'\n\
                 case \" $* \" in *\" {ASSIGNED_FLAG} \"*) ;; *) exec sleep 30 ;; esac\n\
                 command -v python3 > '{python}' || true\n\
                 while read -r command argument < '{commands}'; do\n\
                 case \"$command\" in\n\
                 report)\n\
                 printf '{{\"session_id\":\"%s\"}}' \"$argument\" | sh '{adapter}' session\n\
                 printf '%s\\n' \"$argument\" >> '{acks}' ;;\n\
                 nested)\n\
                 sleep 30 &\n\
                 printf '%s\\n' \"$!\" > '{nested}'\n\
                 printf 'nested\\n' >> '{acks}' ;;\n\
                 exit) exit 0 ;;\n\
                 esac\n\
                 done\n",
                interpreter = interpreter.display(),
                marker = marker.display(),
                python = python.display(),
                commands = commands_path.display(),
                adapter = adapter.display(),
                acks = acks.display(),
                nested = nested.display(),
            ),
        );
        let store_path = temp_store_path(tag);
        let mut config = SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir,
            ..SessionRegistryConfig::default()
        };
        let (runtime_root, state_root) = test_worker_roots(&config);
        config.supervision = Some(crate::session::test_supervision(&runtime_root, &state_root));
        config.worker_runtime_root = Some(runtime_root.clone());
        config.worker_state_root = Some(state_root.clone());
        let launcher: Arc<dyn crate::runtime::WorkerLauncher> = Arc::new(
            crate::runtime::InProcessWorkerLauncher::new(runtime_root, state_root),
        );
        let registry = Self::registry(&config, &launcher, &script, existence);
        let session = registry
            .create(SessionNewParams {
                agent: agent.to_owned(),
                cwd: Some(dir.clone()),
                ..params()
            })
            .await
            .expect("create an assigned session");
        let assigned = session
            .native_session_id
            .clone()
            .expect("the session holds its assigned reference at once");
        Self {
            marker,
            acks,
            nested,
            python,
            script,
            commands,
            store_path,
            config,
            launcher,
            existence,
            registry,
            session,
            assigned,
        }
    }

    fn registry(
        config: &SessionRegistryConfig,
        launcher: &Arc<dyn crate::runtime::WorkerLauncher>,
        script: &std::path::Path,
        existence: &str,
    ) -> SessionRegistry {
        SessionRegistry::new_with_runtimes_and_launcher(
            config.clone(),
            crate::agent::host::fixture::pi_shaped_hooked_host(script, existence),
            Arc::clone(launcher),
        )
    }

    fn id(&self) -> &SessionId {
        &self.session.id
    }

    /// Writes one command line for the agent.
    fn send(&self, line: &str) {
        let mut writer = &self.commands;
        writer
            .write_all(format!("{line}\n").as_bytes())
            .expect("write the agent command");
    }

    /// Has the agent report conversation `reference` through the real adapter
    /// and returns once the worker answered.
    async fn report(&self, reference: &str) {
        self.send(&format!("report {reference}"));
        wait_for_file_contains(&self.acks, &format!("{reference}\n")).await;
        let python = fs::read_to_string(&self.python).unwrap_or_default();
        assert!(
            !python.trim().is_empty(),
            "the adapter needs python3 on PATH"
        );
    }

    /// The worker's current snapshot, as the daemon polls it.
    async fn snapshot(
        &self,
        registry: &SessionRegistry,
    ) -> pohunek_worker_protocol::InspectSnapshot {
        let (worker, _identity) = live_worker_and_identity(registry, self.id()).await;
        worker.inspect().await.expect("inspect the worker")
    }

    /// Applies `snapshot` and returns the settled outcome.
    async fn settle_with(
        &self,
        registry: &SessionRegistry,
        snapshot: &pohunek_worker_protocol::InspectSnapshot,
    ) -> WorkerMetadataApplyOutcome {
        wait_until("the daemon to settle the worker snapshot", || async {
            match Box::pin(registry.apply_worker_metadata_snapshot(self.id(), snapshot)).await {
                WorkerMetadataApplyOutcome::Retryable(_) => None,
                settled => Some(settled),
            }
        })
        .await
    }

    async fn settle(&self, registry: &SessionRegistry) -> WorkerMetadataApplyOutcome {
        let snapshot = self.snapshot(registry).await;
        self.settle_with(registry, &snapshot).await
    }

    fn durable_record(&self) -> crate::store::SessionRecord {
        crate::store::Store::new(self.store_path.clone())
            .load_sessions()
            .expect("durable sessions")
            .into_iter()
            .find(|record| record.session_id == self.id().0)
            .expect("durable record")
    }

    /// Asserts that the session, its in-memory launch snapshot, its durable
    /// record and its resume binding all hold `reference` with `provenance`.
    async fn assert_held(
        &self,
        registry: &SessionRegistry,
        reference: &str,
        provenance: NativeReferenceProvenance,
    ) {
        let info = registry.inspect(self.id()).await.expect("inspect");
        assert_eq!(
            info.native_session_id.as_deref(),
            Some(reference),
            "session"
        );
        let memory = registry
            .inner
            .sessions
            .lock()
            .await
            .get(self.id())
            .map(|entry| entry.snapshot.reference_provenance)
            .expect("live entry");
        assert_eq!(memory, provenance, "in-memory provenance");
        let record = self.durable_record();
        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some(reference),
            "durable session"
        );
        let recovery = record.recovery.expect("durable recovery binding");
        assert_eq!(
            recovery.native_session_id.as_deref(),
            Some(reference),
            "durable binding"
        );
        assert_eq!(
            recovery.native_reference_provenance, provenance,
            "durable provenance"
        );
        let legacy = crate::store::Store::new(self.store_path.clone())
            .load_resume()
            .expect("resume store")
            .into_iter()
            .find(|binding| binding.session_id == self.id().0)
            .expect("resume binding");
        assert_eq!(
            legacy.native_session_id.as_deref(),
            Some(reference),
            "resume binding"
        );
        assert_eq!(
            legacy.native_reference_provenance, provenance,
            "resume provenance"
        );
    }

    /// A restarted daemon: a second registry over the same store, roots and
    /// workers adopts the live session after the first one is gone.
    async fn restart(&mut self) -> SessionRegistry {
        let entry = self
            .registry
            .inner
            .sessions
            .lock()
            .await
            .remove(self.id())
            .expect("committed session entry");
        entry.cancel_runtime_watchers();
        drop(entry);
        let restarted = Self::registry(&self.config, &self.launcher, &self.script, self.existence);
        restarted
            .reconcile_workers()
            .await
            .expect("startup reconciliation");
        drop(std::mem::replace(&mut self.registry, restarted.clone()));
        restarted
    }

    /// Ends the agent and waits until the session is terminal.
    async fn exit(&self, registry: &SessionRegistry) {
        self.send("exit");
        registry
            .wait_for_exit(self.id(), HANG_GUARD)
            .await
            .expect("the agent exits");
    }

    fn launch_argv(&self, index: usize) -> Vec<String> {
        recorded_launch(&self.marker, index)
    }

    async fn finish(&self, registry: &SessionRegistry) {
        let _ = registry.stop(self.id()).await;
    }
}

fn fork_params(id: &SessionId) -> SessionForkParams {
    SessionForkParams {
        session_id: id.clone(),
        name: None,
        cwd_mode: ForkCwdMode::Same,
        cols: 80,
        rows: 24,
    }
}

#[tokio::test]
async fn a_launch_claim_naming_the_assigned_reference_turns_it_reported() {
    let rig = Rig::new(
        "supersede-confirm",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.assert_held(
        &rig.registry,
        &rig.assigned,
        NativeReferenceProvenance::Assigned,
    )
    .await;

    // The agent confirms the conversation it was told to use. Only its launch
    // claim is imported, so nothing but the provenance changes.
    rig.report(&rig.assigned).await;
    let mut snapshot = rig.snapshot(&rig.registry).await;
    assert!(
        snapshot.launch_identity.is_some(),
        "the worker accepted the claim"
    );
    snapshot.active_identity = None;
    assert_eq!(
        rig.settle_with(&rig.registry, &snapshot).await,
        WorkerMetadataApplyOutcome::Applied
    );

    rig.assert_held(
        &rig.registry,
        &rig.assigned,
        NativeReferenceProvenance::Reported,
    )
    .await;
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn a_launch_claim_naming_another_conversation_replaces_the_assigned_reference() {
    let rig = Rig::new(
        "supersede-launch",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;

    // The user switched conversation before the agent's first report.
    rig.report("conversation-after-clear").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied,
        "a reported reference supersedes the assigned one instead of conflicting"
    );

    assert_ne!(rig.assigned, "conversation-after-clear");
    rig.assert_held(
        &rig.registry,
        "conversation-after-clear",
        NativeReferenceProvenance::Reported,
    )
    .await;
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn a_conversation_switch_after_the_launch_claim_survives_a_restart_a_resume_and_a_fork() {
    let mut rig = Rig::new(
        "supersede-clear",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report(&rig.assigned).await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.assert_held(
        &rig.registry,
        &rig.assigned,
        NativeReferenceProvenance::Reported,
    )
    .await;

    // `/clear`: the worker rejects every launch claim after its first, so the
    // new conversation arrives as the agent's active identity.
    rig.report("conversation-cleared").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.assert_held(
        &rig.registry,
        "conversation-cleared",
        NativeReferenceProvenance::Reported,
    )
    .await;
    assert!(
        rig.durable_record().native_identity_ordering.is_some(),
        "the switch is ordered like any reported identity"
    );
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied,
        "the worker still holds its first launch claim; the same snapshot stays applied"
    );
    rig.assert_held(
        &rig.registry,
        "conversation-cleared",
        NativeReferenceProvenance::Reported,
    )
    .await;

    // A restarted daemon keeps the switched reference and never goes back to
    // the assigned one, whether it adopts, re-imports or re-persists.
    let restarted = rig.restart().await;
    rig.assert_held(
        &restarted,
        "conversation-cleared",
        NativeReferenceProvenance::Reported,
    )
    .await;
    assert_eq!(
        rig.settle(&restarted).await,
        WorkerMetadataApplyOutcome::Applied
    );
    restarted
        .resize(rig.id(), 100, 30)
        .await
        .expect("a resize re-persists the binding");
    rig.assert_held(
        &restarted,
        "conversation-cleared",
        NativeReferenceProvenance::Reported,
    )
    .await;

    // Recovery and fork launch from the switched conversation. A reported
    // reference is the agent's own word, so the declared existence check does
    // not run for it.
    rig.exit(&restarted).await;
    restarted.resume(rig.id()).await.expect("native recovery");
    wait_for_file_contains(&rig.marker, "--session\n").await;
    assert_eq!(
        rig.launch_argv(1),
        ["--model", "fast", "--session", "conversation-cleared"]
    );
    rig.assert_held(
        &restarted,
        "conversation-cleared",
        NativeReferenceProvenance::Reported,
    )
    .await;
    let forked = restarted
        .fork(fork_params(rig.id()))
        .await
        .expect("native fork");
    wait_for_file_contains(&rig.marker, "--fork\n").await;
    assert_eq!(
        rig.launch_argv(2),
        ["--model", "fast", "--fork", "conversation-cleared"]
    );
    assert_eq!(
        forked.native_session_id, None,
        "a fork learns its own reference"
    );
    let _ = restarted.stop(&forked.id).await;
    rig.finish(&restarted).await;
}

/// The public report for conversation `reference`, with every identity field
/// the caller does not override taken from the live worker.
async fn public_report(
    rig: &Rig,
    reference: &str,
    adjust: impl FnOnce(&mut PublicReport),
) -> SessionReportNativeIdParams {
    let snapshot = rig.snapshot(&rig.registry).await;
    let process = snapshot.child_process.expect("worker child");
    let mut report = PublicReport {
        worker_instance_id: snapshot
            .worker_instance_id
            .expect("worker instance")
            .to_string(),
        pid: process.pid,
        pid_start_identity: process.start_identity,
        sequence: NATIVE_REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        expires_at: native_report_expiry(),
    };
    adjust(&mut report);
    SessionReportNativeIdParams::new(
        rig.id().clone(),
        report.worker_instance_id,
        RUNTIME.to_owned(),
        report.pid,
        ProcessStartIdentity::new(report.pid_start_identity),
        ReportSequence::new(report.sequence),
        report.expires_at,
        reference.to_owned(),
        None,
    )
    .expect("valid native identity report")
}

/// The identity fields of a public report a test may corrupt.
struct PublicReport {
    worker_instance_id: String,
    pid: u32,
    pid_start_identity: u64,
    sequence: u64,
    expires_at: String,
}

#[tokio::test]
async fn a_public_report_replaces_the_assigned_reference_in_sequence_order_and_recovery_uses_it() {
    let rig = Rig::new(
        "supersede-public",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;

    // A report that cannot prove its process, runtime or freshness leaves the
    // assigned reference alone.
    let wrong_start = public_report(&rig, "forged-start", |report| {
        report.pid_start_identity += 1;
    })
    .await;
    assert!(!rig.registry.report_native_id(wrong_start).await.recorded);
    let wrong_pid = public_report(&rig, "forged-pid", |report| report.pid = 1).await;
    assert!(!rig.registry.report_native_id(wrong_pid).await.recorded);
    let wrong_worker = public_report(&rig, "forged-worker", |report| {
        report.worker_instance_id = "another-worker-instance".to_owned();
    })
    .await;
    assert!(!rig.registry.report_native_id(wrong_worker).await.recorded);
    let expired = public_report(&rig, "expired", |report| {
        report.expires_at = (OffsetDateTime::now_utc() - time::Duration::seconds(5))
            .format(&Rfc3339)
            .expect("format expiry");
    })
    .await;
    assert!(!rig.registry.report_native_id(expired).await.recorded);
    rig.assert_held(
        &rig.registry,
        &rig.assigned,
        NativeReferenceProvenance::Assigned,
    )
    .await;

    // `/clear` twice: each valid report replaces value and provenance.
    let first = public_report(&rig, "conversation-one", |_| {}).await;
    assert!(rig.registry.report_native_id(first).await.recorded);
    rig.assert_held(
        &rig.registry,
        "conversation-one",
        NativeReferenceProvenance::Reported,
    )
    .await;
    let second = public_report(&rig, "conversation-two", |_| {}).await;
    assert!(rig.registry.report_native_id(second).await.recorded);
    rig.assert_held(
        &rig.registry,
        "conversation-two",
        NativeReferenceProvenance::Reported,
    )
    .await;

    // A report older than the accepted one never replaces it.
    let accepted = rig
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(rig.id())
        .and_then(|entry| entry.last_native_report.clone())
        .expect("the accepted report is ordered");
    let stale = public_report(&rig, "conversation-stale", |report| {
        report.sequence = accepted.sequence - 1;
    })
    .await;
    assert!(!rig.registry.report_native_id(stale).await.recorded);
    rig.assert_held(
        &rig.registry,
        "conversation-two",
        NativeReferenceProvenance::Reported,
    )
    .await;

    // Recovery and fork use the switched conversation.
    rig.exit(&rig.registry).await;
    rig.registry
        .resume(rig.id())
        .await
        .expect("native recovery");
    wait_for_file_contains(&rig.marker, "--session\n").await;
    assert_eq!(
        rig.launch_argv(1),
        ["--model", "fast", "--session", "conversation-two"]
    );
    rig.assert_held(
        &rig.registry,
        "conversation-two",
        NativeReferenceProvenance::Reported,
    )
    .await;
    let forked = rig
        .registry
        .fork(fork_params(rig.id()))
        .await
        .expect("native fork");
    wait_for_file_contains(&rig.marker, "--fork\n").await;
    assert_eq!(
        rig.launch_argv(2),
        ["--model", "fast", "--fork", "conversation-two"]
    );
    let _ = rig.registry.stop(&forked.id).await;
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn an_active_identity_that_is_not_the_launch_process_leaves_the_assigned_reference() {
    let rig = Rig::new(
        "supersede-gating",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.send("nested");
    wait_for_file_contains(&rig.acks, "nested\n").await;
    let nested_pid: u32 = fs::read_to_string(&rig.nested)
        .expect("nested pid")
        .trim()
        .parse()
        .expect("nested pid number");
    let nested_start = crate::procwatch::HostInspector::new()
        .identity(nested_pid)
        .expect("inspect the nested process")
        .expect("the nested process is live")
        .start_identity
        .get();
    let base = rig.snapshot(&rig.registry).await;
    let root = base.child_process.expect("worker child");
    let claim = |provider: &str, process: pohunek_worker_protocol::ProcessIdentity| {
        pohunek_worker_protocol::ActiveIdentityClaim {
            provider: provider.to_owned(),
            process,
            sequence: u64::MAX / 2,
            expires_at: native_report_expiry(),
            reference_kind: Some("id".to_owned()),
            native_reference: Some("another-conversation".to_owned()),
        }
    };

    // A nested agent of the same runtime and another runtime's claim are not
    // the launch process's own conversation.
    for (name, active) in [
        (
            "a nested process of the runtime",
            claim(
                RUNTIME,
                pohunek_worker_protocol::ProcessIdentity {
                    pid: nested_pid,
                    start_identity: nested_start,
                },
            ),
        ),
        (
            "another runtime on the launch process",
            claim("claude", root),
        ),
    ] {
        let mut snapshot = base.clone();
        snapshot.launch_identity = None;
        snapshot.active_identity = Some(active);
        rig.settle_with(&rig.registry, &snapshot).await;
        rig.assert_held(
            &rig.registry,
            &rig.assigned,
            NativeReferenceProvenance::Assigned,
        )
        .await;
        assert!(
            rig.durable_record().native_identity_ordering.is_none(),
            "{name}: nothing was ordered"
        );
    }

    // The launch process itself replaces it.
    let mut snapshot = base;
    snapshot.launch_identity = None;
    snapshot.active_identity = Some(claim(RUNTIME, root));
    assert_eq!(
        rig.settle_with(&rig.registry, &snapshot).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.assert_held(
        &rig.registry,
        "another-conversation",
        NativeReferenceProvenance::Reported,
    )
    .await;
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn recovery_checks_an_assigned_reference_but_trusts_a_reported_one() {
    let home = temp_dir("supersede-existence-home");
    let tag = "supersede-existence";
    let agents_dir = temp_agents_dir_with(
        tag,
        "pi-home",
        &format!(
            "base = \"pi\"\n{}[env]\n{} = \"{}\"\n",
            crate::agent::host::fixture::pi_shaped_profile_pin(),
            crate::agent::host::fixture::PI_SHAPED_HOME_ENV,
            home.display()
        ),
    );
    let first = Rig::with_agents(
        tag,
        crate::agent::host::fixture::PI_SHAPED_FILE_CHECK,
        Some(agents_dir),
        "pi-home",
    )
    .await;

    // Still assigned and never written by the agent: refused.
    first.exit(&first.registry).await;
    let refused = first
        .registry
        .resume(first.id())
        .await
        .expect_err("an assigned reference needs its conversation");
    assert_eq!(refused.code, "agent_native_reference_missing");

    // The agent reports another conversation; recovery trusts the agent's word
    // and does not look for a file it never promised.
    let second = Rig::with_agents(
        "supersede-existence-reported",
        crate::agent::host::fixture::PI_SHAPED_FILE_CHECK,
        first.config.agents_dir.clone(),
        "pi-home",
    )
    .await;
    let report = public_report(&second, "conversation-without-file", |_| {}).await;
    assert!(second.registry.report_native_id(report).await.recorded);
    second.exit(&second.registry).await;
    second
        .registry
        .resume(second.id())
        .await
        .expect("a reported reference is recovered unchecked");
    wait_for_file_contains(&second.marker, "--session\n").await;
    assert_eq!(
        second.launch_argv(1),
        ["--session", "conversation-without-file"],
        "a profile recovers with its own args and the resume template"
    );
    second.finish(&second.registry).await;
}
