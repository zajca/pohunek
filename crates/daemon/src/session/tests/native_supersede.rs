//! A reported native reference supersedes an assigned one, over real session
//! workers and a Pi-shaped runtime whose agent reports its conversation the way
//! a hooked agent does when the user switches conversations (`/clear`).

// Rust guideline compliant 2026-10-05

use super::*;
use crate::agent::NativeReferenceProvenance;

/// The reference the launch hands to the agent in its argv, as the fixture
/// agent sees it.
const ASSIGNED_FLAG: &str = "--session-id";

/// How long a test waits for the worker to journal a reference its agent
/// reported. Process verification answers within milliseconds when it can
/// succeed, so a longer wait only delays the diagnostic.
const JOURNAL_DEADLINE: Duration = Duration::from_secs(30);

/// The pending launch claims journaled for `session`: whether each may still be
/// retried, which process it names and when it expires.
fn pending_claims(config: &SessionRegistryConfig, session: &str) -> String {
    let Some(root) = config.worker_state_root.as_ref() else {
        return "no worker state root".to_owned();
    };
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(journal) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if journal["session_id"] == session {
                    let claims = journal["pending_launch_claims"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|claim| {
                            format!(
                                "retry_pending={} process={} expires_at={}",
                                claim["retry_pending"],
                                claim["identity"]["process"],
                                claim["expires_at"]
                            )
                        })
                        .collect::<Vec<_>>();
                    found.push(format!("{}: {claims:?}", path.display()));
                }
            }
        }
    }
    found.join("; ")
}

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
    dir: PathBuf,
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
    /// The worker's handle while no daemon holds the session.
    orphan: Option<Worker>,
}

impl Rig {
    async fn new(tag: &str, existence: &'static str) -> Self {
        Self::with_agents(tag, existence, None, RUNTIME, false).await
    }

    /// An agent started below a wrapper process, as launchers and shims do.
    async fn wrapped(tag: &str, existence: &'static str) -> Self {
        Self::with_agents(tag, existence, None, RUNTIME, true).await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one fixture builds the agent script, its roots and its registry"
    )]
    async fn with_agents(
        tag: &str,
        existence: &'static str,
        agents_dir: Option<PathBuf>,
        agent: &str,
        wrapped: bool,
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
        let inner = dir.join("pi-switching");
        write_executable(
            &inner,
            &format!(
                "#!{interpreter}\n\
                 printf 'launch\\n' >> '{marker}'\n\
                 printf '%s\\n' \"$@\" >> '{marker}'\n\
                 case \" $* \" in *\" {ASSIGNED_FLAG} \"*|*\" --session \"*) ;; *) exec sleep 30 ;; esac\n\
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
        // The wrapper keeps the root; the worker designates the provider
        // process below it as the launch process.
        let script = if wrapped {
            let shim = dir.join("pi-wrapper");
            write_executable(
                &shim,
                &format!(
                    "#!/bin/sh\n'{}' '{}' \"$@\"\nif [ -e '{}' ]; then exec sleep 30; fi\n",
                    interpreter.display(),
                    inner.display(),
                    dir.join("linger").display()
                ),
            );
            shim
        } else {
            inner
        };
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
        // The launch records its argv line by line; later launches are counted
        // against a settled first one.
        wait_for_file_contains(&marker, &format!("{assigned}\n")).await;
        Self {
            dir,
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
            orphan: None,
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
            Arc::new(ReadableHost::new()),
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
        // The worker verifies the launch process asynchronously when the
        // process table is slow to answer; the reference is journaled once it
        // has, so the snapshots the tests apply always carry it.
        let worker = match &self.orphan {
            Some(worker) => worker.clone(),
            None => live_worker_and_identity(&self.registry, self.id()).await.0,
        };
        let journaled = tokio::time::timeout(
            JOURNAL_DEADLINE,
            wait_until("the worker to journal the reported reference", || async {
                let snapshot = worker.inspect().await.ok()?;
                snapshot
                    .native_reference
                    .filter(|journaled| journaled.native_reference == reference)
                    .map(|_| ())
            }),
        )
        .await;
        assert!(
            journaled.is_ok(),
            "the worker did not journal the reported reference within {JOURNAL_DEADLINE:?}\n{}",
            self.launch_diagnostic(&worker).await
        );
        let python = fs::read_to_string(&self.python).unwrap_or_default();
        assert!(
            !python.trim().is_empty(),
            "the adapter needs python3 on PATH"
        );
    }

    /// What the worker and the host process table say about the launch
    /// process, for a failure that must explain itself.
    async fn launch_diagnostic(&self, worker: &Worker) -> String {
        use std::fmt::Write as _;

        let mut report = String::new();
        match worker.inspect().await {
            Ok(snapshot) => {
                let _ = writeln!(
                    report,
                    "snapshot: phase={:?} child={:?}\n  launch_identity={:?}\n  native_reference={:?}\n  active_identity={:?}",
                    snapshot.phase,
                    snapshot.child_process,
                    snapshot.launch_identity,
                    snapshot.native_reference,
                    snapshot.active_identity,
                );
                if let Some(root) = snapshot.child_process {
                    let inspector = crate::procwatch::HostInspector::new();
                    let identity = ProcessIdentity {
                        pid: root.pid,
                        start_identity: StartIdentity::new(root.start_identity),
                    };
                    let _ = writeln!(
                        report,
                        "host: identity({})={:?}, root start identity {}",
                        root.pid,
                        inspector.identity(root.pid),
                        root.start_identity
                    );
                    let mut processes = vec![identity];
                    match inspector.descendant_identities(identity) {
                        Ok(descendants) => processes.extend(descendants),
                        Err(error) => {
                            let _ = writeln!(report, "host: descendants failed: {error}");
                        }
                    }
                    for process in processes {
                        let _ = writeln!(
                            report,
                            "host: pid {} start {} executable={:?} parent={:?}",
                            process.pid,
                            process.start_identity.get(),
                            inspector.executable(process.pid),
                            inspector.parent_pid(process.pid),
                        );
                    }
                }
            }
            Err(error) => {
                let _ = writeln!(report, "snapshot unavailable: {error}");
            }
        }
        let _ = writeln!(
            report,
            "pending claims in the worker journal: {}",
            pending_claims(&self.config, &self.session.id.0)
        );
        report
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
        // The replaced registry stands for the dead daemon: once its last
        // in-flight projection write is done, the lock stays held so a late
        // write of its own cannot remove the binding the new daemon owns.
        let barrier = self.registry.inner.persist_lock.lock().await;
        std::mem::forget(barrier);
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
        // The dead daemon's controller connection is gone before the new one
        // connects.
        self.orphan = None;
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
        accept_profile_change: false,
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
        report.sequence = accepted.sequence.expect("public mark") - 1;
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
        false,
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
        false,
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

#[tokio::test]
async fn a_switch_before_the_first_report_after_a_resume_survives_a_restart() {
    let mut rig = Rig::new(
        "supersede-resume-switch",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report(&rig.assigned).await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.exit(&rig.registry).await;
    rig.registry
        .resume(rig.id())
        .await
        .expect("native recovery");
    wait_for_file_contains(&rig.marker, "--session\n").await;

    // The recovered process first reports another conversation: its launch
    // claim differs from the reference the recovery relaunched with.
    rig.report("conversation-after-resume").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied,
        "the new generation's claim supersedes the recovered reference"
    );
    rig.assert_held(
        &rig.registry,
        "conversation-after-resume",
        NativeReferenceProvenance::Reported,
    )
    .await;

    let restarted = rig.restart().await;
    rig.assert_held(
        &restarted,
        "conversation-after-resume",
        NativeReferenceProvenance::Reported,
    )
    .await;
    assert_eq!(
        rig.settle(&restarted).await,
        WorkerMetadataApplyOutcome::Applied,
        "a restart adopts the session instead of marking it in conflict"
    );
    let info = restarted.inspect(rig.id()).await.expect("inspect");
    assert_ne!(
        info.runtime.as_ref().map(|runtime| runtime.state),
        Some(RuntimeState::Conflict)
    );
    rig.finish(&restarted).await;
}

#[tokio::test]
async fn a_transition_that_captured_the_assigned_entry_keeps_the_reported_provenance() {
    let rig = Rig::new(
        "supersede-transition",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report(&rig.assigned).await;
    let mut snapshot = rig.snapshot(&rig.registry).await;
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

    // An exit transition built its candidate from the entry before the
    // snapshot reached memory: the entry still says assigned while the
    // durable record already says reported.
    let (_worker, identity) = live_worker_and_identity(&rig.registry, rig.id()).await;
    let mut sessions = rig.registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(rig.id()).expect("session entry");
    entry.runtime_watch_cancel.cancel();
    entry.snapshot.reference_provenance = NativeReferenceProvenance::Assigned;
    entry.last_native_report = None;
    drop(sessions);
    // A concurrent live-state commit may make the transition retry.
    wait_until("the exit transition to commit", || async {
        rig.registry
            .record_exit(
                rig.id(),
                RuntimeExit {
                    exit_code: Some(0),
                    success: true,
                },
                false,
                Some(&identity),
                None,
            )
            .await
            .expect("record the exit")
            .then_some(())
    })
    .await;

    let memory = rig
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(rig.id())
        .map(|entry| entry.snapshot.reference_provenance)
        .expect("entry");
    assert_eq!(memory, NativeReferenceProvenance::Reported);
    assert_eq!(
        rig.durable_record()
            .recovery
            .expect("durable recovery binding")
            .native_reference_provenance,
        NativeReferenceProvenance::Reported,
        "the transition did not demote the committed provenance"
    );
}

#[tokio::test]
async fn a_switch_below_a_wrapper_is_followed_through_the_verified_launch_process() {
    let rig = Rig::wrapped(
        "supersede-wrapper",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report(&rig.assigned).await;
    let snapshot = rig.snapshot(&rig.registry).await;
    let launch = snapshot.launch_identity.as_ref().expect("launch claim");
    assert_ne!(
        Some(launch.process),
        snapshot.child_process,
        "the provider process runs below the wrapper"
    );
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );

    rig.report("conversation-below-wrapper").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.assert_held(
        &rig.registry,
        "conversation-below-wrapper",
        NativeReferenceProvenance::Reported,
    )
    .await;
    rig.exit(&rig.registry).await;
    rig.registry
        .resume(rig.id())
        .await
        .expect("native recovery");
    wait_for_file_contains(&rig.marker, "--session\n").await;
    assert_eq!(
        rig.launch_argv(1),
        ["--model", "fast", "--session", "conversation-below-wrapper"]
    );
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn worker_and_public_reports_are_ordered_within_their_own_clock() {
    let rig = Rig::new(
        "supersede-transports",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    let held = |reference: &'static str| {
        rig.assert_held(
            &rig.registry,
            reference,
            NativeReferenceProvenance::Reported,
        )
    };

    // Public first, then the worker: the adapter's worker clock is far ahead
    // of its public one, and neither blocks the other.
    let public = public_report(&rig, "public-one", |_| {}).await;
    assert!(rig.registry.report_native_id(public).await.recorded);
    held("public-one").await;
    rig.report("worker-two").await;
    let old_claim = rig.snapshot(&rig.registry).await;
    assert!(
        old_claim.active_identity.is_some(),
        "the worker journaled the claim"
    );
    assert_eq!(
        rig.settle_with(&rig.registry, &old_claim).await,
        WorkerMetadataApplyOutcome::Applied
    );
    held("worker-two").await;

    // The worker, then the public fallback with a smaller number.
    let public = public_report(&rig, "public-three", |_| {}).await;
    assert!(
        rig.registry.report_native_id(public).await.recorded,
        "a public report is not stale because the worker clock reads larger"
    );
    held("public-three").await;

    // The worker's older claim is not newer than the public report it
    // preceded, so importing it again leaves the newer conversation.
    assert_eq!(
        rig.settle_with(&rig.registry, &old_claim).await,
        WorkerMetadataApplyOutcome::Applied
    );
    held("public-three").await;

    // Each clock still rejects its own stale numbers.
    let accepted = rig
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(rig.id())
        .and_then(|entry| entry.last_native_report.clone())
        .expect("ordered report");
    let stale = public_report(&rig, "public-stale", |report| {
        report.sequence = accepted.sequence.expect("public mark");
    })
    .await;
    assert!(!rig.registry.report_native_id(stale).await.recorded);

    rig.report("worker-four").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    held("worker-four").await;

    // A public report numbered below the accepted public one is stale even
    // though a worker claim was accepted after it, and never replaces the
    // newer conversation.
    let late = public_report(&rig, "public-late", |report| {
        report.sequence = accepted.sequence.expect("public mark");
    })
    .await;
    assert!(!rig.registry.report_native_id(late).await.recorded);
    held("worker-four").await;
    let public = public_report(&rig, "public-five", |_| {}).await;
    assert!(rig.registry.report_native_id(public).await.recorded);
    held("public-five").await;
    rig.finish(&rig.registry).await;
}

/// Lines of a recovery launch the fixture agent records: `--model fast`, the
/// recovery flag and the reference.
const LAUNCH_ARGV_LINES: usize = 4;

/// One step of a generated history of a session whose reference core assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The agent reports a new conversation through the worker.
    Switch,
    /// Like [`Step::Switch`], read after the active claim's lease expired.
    SwitchAfterLease,
    /// A new conversation reaches the daemon socket directly.
    PublicReport,
    /// A public report numbered like the last accepted one.
    StalePublicReport,
    /// A worker snapshot from before the latest report is applied again.
    ReplayOlderSnapshot,
    /// Every later resume-binding write is lost, as in a crash after the
    /// record write.
    LoseProjectionWrites,
    /// The daemon restarts and adopts the live worker.
    Restart,
    /// The daemon is down while the agent switches, then restarts.
    SwitchWhileDown,
    /// The agent exits and is resumed from its latest reference.
    ExitAndResume,
    /// The latest conversation is forked.
    Fork,
    /// The agent switches while no daemon runs and exits before the daemon
    /// returns; the ended session is then resumed.
    SwitchDownThenExit,
    /// The agent switches and the registry's own watcher imports it.
    SwitchViaWatcher,
    /// A public report numbered 0, admitted only while no public report was
    /// accepted for the generation.
    PublicSequenceZero,
}

const STEPS: [Step; 13] = [
    Step::Switch,
    Step::SwitchAfterLease,
    Step::PublicReport,
    Step::StalePublicReport,
    Step::ReplayOlderSnapshot,
    Step::LoseProjectionWrites,
    Step::Restart,
    Step::SwitchWhileDown,
    Step::ExitAndResume,
    Step::Fork,
    Step::SwitchDownThenExit,
    Step::SwitchViaWatcher,
    Step::PublicSequenceZero,
];

/// Deterministic generator of step histories: a 64-bit xorshift stream, so a
/// failing case is reproduced by its seed alone.
struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn history(&mut self, length: usize) -> Vec<Step> {
        (0..length)
            .map(|_| STEPS[usize::try_from(self.next() % STEPS.len() as u64).expect("index")])
            .collect()
    }
}

impl Rig {
    /// Quiesces the registry standing for the dead daemon, optionally has the
    /// agent switch to `reference` while no daemon runs, then starts a new
    /// daemon over the same store and workers.
    async fn restart_with(&mut self, while_down: Option<&str>) -> SessionRegistry {
        let barrier = self.registry.inner.persist_lock.lock().await;
        std::mem::forget(barrier);
        // Every session of the dead daemon lets go of its worker, forks
        // included, or a worker would keep serving its former controller.
        self.orphan = Some(live_worker_and_identity(&self.registry, self.id()).await.0);
        let entries: Vec<_> = self
            .registry
            .inner
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.cancel_runtime_watchers();
        }
        if let Some(reference) = while_down {
            self.report(reference).await;
        }
        // The dead daemon's controller connection is gone before the new one
        // connects.
        self.orphan = None;
        let restarted = Self::registry(&self.config, &self.launcher, &self.script, self.existence);
        restarted
            .reconcile_workers()
            .await
            .expect("startup reconciliation");
        drop(std::mem::replace(&mut self.registry, restarted.clone()));
        restarted
    }

    /// The agent switches to `reference` and exits while no daemon runs; the
    /// new daemon then finds an ended worker.
    async fn restart_down_then_exit(&mut self, reference: &str) {
        let barrier = self.registry.inner.persist_lock.lock().await;
        std::mem::forget(barrier);
        self.orphan = Some(live_worker_and_identity(&self.registry, self.id()).await.0);
        let entries: Vec<_> = self
            .registry
            .inner
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.cancel_runtime_watchers();
        }
        self.report(reference).await;
        self.send("exit");
        // The dead daemon's controller connection is gone before the new one
        // connects.
        self.orphan = None;
        let restarted = Self::registry(&self.config, &self.launcher, &self.script, self.existence);
        restarted
            .reconcile_workers()
            .await
            .expect("startup reconciliation");
        wait_until("the ended session to be imported", || async {
            let info = restarted.inspect(self.id()).await.ok()?;
            matches!(info.state, SessionState::Done | SessionState::Failed).then_some(())
        })
        .await;
        drop(std::mem::replace(&mut self.registry, restarted));
    }

    fn launches(&self) -> usize {
        fs::read_to_string(&self.marker).map_or(0, |text| text.matches("launch\n").count())
    }

    /// The session's ordering key as the registry holds it.
    async fn last_report(&self) -> Option<crate::store::NativeIdentityOrdering> {
        self.registry
            .inner
            .sessions
            .lock()
            .await
            .get(self.id())
            .and_then(|entry| entry.last_native_report.clone())
    }
}

/// The reference the session holds, in memory and in its durable record, and
/// whether its runtime is in conflict.
async fn held_reference(rig: &Rig) -> (Option<String>, Option<String>, bool) {
    let info = rig.registry.inspect(rig.id()).await.expect("inspect");
    let durable = rig
        .durable_record()
        .recovery
        .and_then(|recovery| recovery.native_session_id);
    let conflict = info
        .runtime
        .as_ref()
        .is_some_and(|runtime| runtime.state == RuntimeState::Conflict);
    (info.native_session_id, durable, conflict)
}

/// Replays `history` against a fresh session and checks, after every step,
/// that the session holds the latest accepted conversation, that no write of
/// its own left a record the next startup rejects, and that resume and fork
/// launch the latest conversation.
#[expect(
    clippy::too_many_lines,
    reason = "one match drives every step kind so the invariants read in one place"
)]
async fn run_history(seed: u64, history: &[Step]) {
    let mut rig = Rig::new(
        &format!("supersede-property-{seed}"),
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    let mut latest = rig.assigned.clone();
    let mut older: Vec<pohunek_worker_protocol::InspectSnapshot> = Vec::new();
    let mut fresh = 0_u32;
    let mut name = |kind: &str| {
        fresh += 1;
        format!("{kind}-{seed}-{fresh}")
    };
    for (index, step) in history.iter().enumerate() {
        let at = format!("seed {seed} step {index} {step:?} of {history:?}");
        match step {
            Step::Switch | Step::SwitchAfterLease => {
                let next = name("worker");
                rig.report(&next).await;
                let mut snapshot = rig.snapshot(&rig.registry).await;
                if *step == Step::SwitchAfterLease {
                    snapshot.active_identity = None;
                    snapshot.active_identity_release = None;
                }
                let outcome = rig.settle_with(&rig.registry, &snapshot).await;
                assert_eq!(outcome, WorkerMetadataApplyOutcome::Applied, "{at}");
                older.push(snapshot);
                latest = next;
            }
            Step::PublicReport => {
                let next = name("public");
                let report = public_report(&rig, &next, |_| {}).await;
                assert!(rig.registry.report_native_id(report).await.recorded, "{at}");
                latest = next;
            }
            Step::StalePublicReport => {
                if let Some(mark) = rig
                    .last_report()
                    .await
                    .map(|key| key.sequence)
                    .filter(|mark| *mark > Some(0))
                {
                    let report = public_report(&rig, &name("stale"), |report| {
                        report.sequence = mark.expect("filtered");
                    })
                    .await;
                    assert!(
                        !rig.registry.report_native_id(report).await.recorded,
                        "{at}"
                    );
                }
            }
            Step::ReplayOlderSnapshot => {
                // Snapshots of an earlier generation are discarded; a current
                // one must never bring back what it said.
                if let Some(snapshot) = older.first() {
                    let _ = rig.settle_with(&rig.registry, snapshot).await;
                }
            }
            Step::LoseProjectionWrites => {
                rig.registry
                    .inner
                    .resume_writes_blocked
                    .store(true, Ordering::Relaxed);
            }
            Step::Restart => {
                rig.restart_with(None).await;
                older.clear();
            }
            Step::SwitchWhileDown => {
                let next = name("down");
                rig.restart_with(Some(&next)).await;
                older.clear();
                latest = next;
            }
            Step::ExitAndResume => {
                let before = rig.launches();
                rig.exit(&rig.registry).await;
                rig.registry.resume(rig.id()).await.expect("resume");
                wait_until("the resumed launch", || async {
                    (rig.launch_argv(before).len() >= LAUNCH_ARGV_LINES).then_some(())
                })
                .await;
                assert_eq!(
                    rig.launch_argv(before),
                    ["--model", "fast", "--session", latest.as_str()],
                    "resume uses the latest reference: {at}"
                );
                older.clear();
            }
            Step::SwitchDownThenExit => {
                let next = name("ended");
                rig.restart_down_then_exit(&next).await;
                older.clear();
                latest = next;
                let before = rig.launches();
                rig.registry.resume(rig.id()).await.expect("resume");
                wait_until("the resumed launch", || async {
                    (rig.launch_argv(before).len() >= LAUNCH_ARGV_LINES).then_some(())
                })
                .await;
                assert_eq!(
                    rig.launch_argv(before),
                    ["--model", "fast", "--session", latest.as_str()],
                    "an ended session resumes the reference it switched to: {at}"
                );
            }
            Step::SwitchViaWatcher => {
                let next = name("watched");
                rig.report(&next).await;
                wait_until("the watcher to import the switch", || async {
                    let info = rig.registry.inspect(rig.id()).await.ok()?;
                    (info.native_session_id.as_deref() == Some(next.as_str())).then_some(())
                })
                .await;
                latest = next;
            }
            Step::PublicSequenceZero => {
                let instance = rig
                    .snapshot(&rig.registry)
                    .await
                    .worker_instance_id
                    .map(|instance| instance.to_string());
                let first_public = rig.last_report().await.is_none_or(|key| {
                    key.sequence.is_none() || Some(key.worker_instance_id) != instance
                });
                let next = name("zero");
                let report = public_report(&rig, &next, |report| report.sequence = 0).await;
                let recorded = rig.registry.report_native_id(report).await.recorded;
                assert_eq!(recorded, first_public, "{at}");
                if recorded {
                    latest = next;
                }
            }
            Step::Fork => {
                let before = rig.launches();
                let forked = rig
                    .registry
                    .fork(fork_params(rig.id()))
                    .await
                    .expect("fork");
                wait_until("the forked launch", || async {
                    (rig.launch_argv(before).len() >= LAUNCH_ARGV_LINES).then_some(())
                })
                .await;
                assert_eq!(
                    rig.launch_argv(before),
                    ["--model", "fast", "--fork", latest.as_str()],
                    "fork uses the latest reference: {at}"
                );
                let _ = rig.registry.stop(&forked.id).await;
            }
        }
        let (memory, durable, conflict) = held_reference(&rig).await;
        assert_eq!(memory.as_deref(), Some(latest.as_str()), "session: {at}");
        assert_eq!(durable.as_deref(), Some(latest.as_str()), "record: {at}");
        assert!(!conflict, "no live worker is quarantined: {at}");
    }
    // A last restart must adopt whatever the history left behind.
    rig.restart_with(None).await;
    let (memory, durable, conflict) = held_reference(&rig).await;
    assert_eq!(memory.as_deref(), Some(latest.as_str()), "final: {seed}");
    assert_eq!(durable.as_deref(), Some(latest.as_str()), "final: {seed}");
    assert!(!conflict, "final restart: {seed} {history:?}");
    rig.finish(&rig.registry).await;
}

/// Declares one test per fixed seed, so a failure names its seed and the
/// histories run in parallel.
macro_rules! generated_history {
    ($($name:ident: $seed:expr,)+) => {$(
        #[tokio::test]
        async fn $name() {
            let history = Generator($seed).history(8);
            run_history($seed, &history).await;
        }
    )+};
}

generated_history! {
    generated_history_one_keeps_the_latest_conversation: 0x9E37_79B9_7F4A_7C15,
    generated_history_two_keeps_the_latest_conversation: 0xD1B5_4A32_D192_ED03,
    generated_history_three_keeps_the_latest_conversation: 0x8CB9_2BA7_2F3D_8DD7,
    generated_history_four_keeps_the_latest_conversation: 0xA076_1D64_78BD_642F,
    generated_history_five_keeps_the_latest_conversation: 0xE703_7ED1_A0B4_28DB,
    generated_history_six_keeps_the_latest_conversation: 0x1D8E_4E27_C47D_124F,
    generated_history_seven_keeps_the_latest_conversation: 0x94D0_49BB_1331_11EB,
    generated_history_eight_keeps_the_latest_conversation: 0xBF58_476D_1CE4_E5B9,
}

#[tokio::test]
async fn generated_histories_over_many_seeds_keep_the_latest_conversation() {
    // Seeds are derived from a fixed one, so a failure names its seed and steps.
    let mut seeds = Generator(0x00C0_FFEE_D15E_A5E5);
    for _ in 0..24 {
        let seed = seeds.next();
        let history = Generator(seed).history(10);
        run_history(seed, &history).await;
    }
}

#[tokio::test]
async fn orderings_found_in_review_hold() {
    use Step::{
        ExitAndResume, Fork, LoseProjectionWrites, PublicReport, PublicSequenceZero,
        ReplayOlderSnapshot, Restart, Switch, SwitchAfterLease, SwitchDownThenExit,
        SwitchViaWatcher, SwitchWhileDown,
    };
    let histories: [&[Step]; 10] = [
        // A crash after the record write, then a resume, then another crash.
        &[Switch, LoseProjectionWrites, Switch, ExitAndResume, Restart],
        &[
            LoseProjectionWrites,
            PublicReport,
            ExitAndResume,
            Restart,
            Fork,
        ],
        // A switch made while the daemon was down, past the lease.
        &[
            Switch,
            SwitchWhileDown,
            SwitchAfterLease,
            Restart,
            ExitAndResume,
        ],
        // A switch before the first report of a recovered generation.
        &[ExitAndResume, SwitchWhileDown, Restart, Fork],
        &[
            Switch,
            ExitAndResume,
            SwitchAfterLease,
            LoseProjectionWrites,
            Restart,
        ],
        &[
            Switch,
            PublicReport,
            ReplayOlderSnapshot,
            Restart,
            ReplayOlderSnapshot,
        ],
        // A switch while the daemon is down, then an exit before it returns.
        &[SwitchDownThenExit, Restart, Fork],
        &[Switch, SwitchDownThenExit, SwitchAfterLease, Restart],
        // The watcher imports a switch that only the durable reference carries.
        &[SwitchViaWatcher, SwitchAfterLease, Restart, ExitAndResume],
        // A worker claim followed by the first public report, numbered 0.
        &[Switch, PublicSequenceZero, PublicSequenceZero, Restart],
    ];
    for (index, history) in histories.iter().enumerate() {
        run_history(1000 + u64::try_from(index).expect("index"), history).await;
    }
}

#[tokio::test]
async fn a_recovery_from_the_stored_binding_keeps_the_ordering_key_of_the_record() {
    let rig = Rig::new(
        "supersede-stored-binding",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report("conversation-kept").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    assert!(rig.durable_record().native_identity_ordering.is_some());
    rig.exit(&rig.registry).await;
    let binding = durable_recovery(&rig.store_path, rig.id());
    rig.registry
        .resume_binding(binding)
        .await
        .expect("recover from the stored binding");
    assert!(
        rig.durable_record().native_identity_ordering.is_some(),
        "the recovered record stays the keyed side of a later reconciliation"
    );
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn a_launch_process_that_already_exited_still_hands_over_its_journaled_reference() {
    let dir = temp_dir("supersede-root-gone");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let inspector = Arc::new(MockInspector::default());
    let registry = SessionRegistry::new_for_test(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            store_path: Some(temp_store_path("supersede-root-gone")),
            ..SessionRegistryConfig::default()
        },
        Arc::<MockInspector>::clone(&inspector),
        Some(crate::agent::host::fixture::pi_shaped_hooked_host(
            &script,
            crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
        )),
        None,
    );
    let created = registry
        .create(SessionNewParams {
            agent: RUNTIME.to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create an assigned session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect the worker");
    let root = snapshot.child_process.expect("worker child");
    snapshot.launch_identity = Some(pohunek_worker_protocol::ReportedLaunchIdentity {
        provider: RUNTIME.to_owned(),
        process: root,
        reference_kind: "id".to_owned(),
        native_reference: "journaled-after-exit".to_owned(),
    });
    snapshot.native_reference = Some(pohunek_worker_protocol::ReportedNativeReference {
        provider: RUNTIME.to_owned(),
        process: root,
        sequence: 5,
        reference_kind: "id".to_owned(),
        native_reference: "journaled-after-exit".to_owned(),
    });
    inspector.set_not_running(ProcessIdentity {
        pid: root.pid,
        start_identity: StartIdentity::new(root.start_identity),
    });

    let _ = registry
        .apply_worker_metadata_snapshot(&created.id, &snapshot)
        .await;

    let info = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        info.native_session_id.as_deref(),
        Some("journaled-after-exit"),
        "the exited process's journaled reference is its own"
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn an_adopted_worker_whose_launch_process_just_exited_hands_over_its_journaled_reference() {
    let mut rig = Rig::new(
        "supersede-drain-adopt",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    let (worker, _identity) = live_worker_and_identity(&rig.registry, rig.id()).await;
    let root = worker
        .inspect()
        .await
        .expect("inspect")
        .child_process
        .expect("worker child");
    rig.orphan = Some(worker);

    // The daemon restarts while the worker is still draining the PTY of a
    // launch process that has already ended.
    let barrier = rig.registry.inner.persist_lock.lock().await;
    std::mem::forget(barrier);
    let entries: Vec<_> = rig
        .registry
        .inner
        .sessions
        .lock()
        .await
        .drain()
        .map(|(_, entry)| entry)
        .collect();
    for entry in entries {
        entry.cancel_runtime_watchers();
    }
    // The agent switches while no daemon holds the session.
    rig.report("conversation-before-exit").await;
    rig.orphan = None;
    let inspector = Arc::new(MockInspector::default());
    inspector.set_not_running(ProcessIdentity {
        pid: root.pid,
        start_identity: StartIdentity::new(root.start_identity),
    });
    let restarted = SessionRegistry::new_with_runtimes_and_launcher(
        rig.config.clone(),
        crate::agent::host::fixture::pi_shaped_hooked_host(&rig.script, rig.existence),
        Arc::clone(&rig.launcher),
        inspector,
    );
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    let info = restarted.inspect(rig.id()).await.expect("inspect");
    assert_eq!(
        info.native_session_id.as_deref(),
        Some("conversation-before-exit")
    );
    let _ = restarted.stop(rig.id()).await;
}

#[tokio::test]
async fn the_launch_diagnostic_names_the_worker_and_host_view_of_the_launch_process() {
    let rig = Rig::new(
        "supersede-diagnostic",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    let (worker, _identity) = live_worker_and_identity(&rig.registry, rig.id()).await;

    let report = rig.launch_diagnostic(&worker).await;

    for expected in [
        "launch_identity=",
        "native_reference=",
        "host: identity(",
        "executable=",
        "pending claims in the worker journal",
    ] {
        assert!(report.contains(expected), "{expected}: {report}");
    }
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn a_switch_survives_the_provider_exiting_before_its_wrapper() {
    let rig = Rig::wrapped(
        "supersede-provider-exits",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report(&rig.assigned).await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    // The registry's watcher is stopped so only the final import can see the
    // switch the worker accepted.
    rig.registry
        .inner
        .sessions
        .lock()
        .await
        .get(rig.id())
        .expect("session entry")
        .runtime_watch_cancel
        .cancel();
    fs::write(rig.dir.join("linger"), "").expect("keep the wrapper alive");
    rig.report("conversation-before-provider-exit").await;
    let snapshot = rig.snapshot(&rig.registry).await;
    rig.send("exit");

    // The provider ends while its wrapper stays: the claim's process is gone
    // and the worker still runs.
    wait_until("the provider to exit", || async {
        let launch = snapshot.launch_identity.as_ref()?.process;
        let gone = !crate::procwatch::HostInspector::new()
            .is_running(ProcessIdentity {
                pid: launch.pid,
                start_identity: StartIdentity::new(launch.start_identity),
            })
            .unwrap_or(true);
        gone.then_some(())
    })
    .await;
    let _ = rig.settle(&rig.registry).await;

    let info = rig.registry.inspect(rig.id()).await.expect("inspect");
    assert_eq!(
        info.native_session_id.as_deref(),
        Some("conversation-before-provider-exit"),
        "the journaled reference needs no live process"
    );
    rig.finish(&rig.registry).await;
}

#[tokio::test]
async fn an_explicit_stop_keeps_the_conversation_the_worker_accepted_before_it() {
    let rig = Rig::new(
        "supersede-stop",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.registry
        .inner
        .sessions
        .lock()
        .await
        .get(rig.id())
        .expect("session entry")
        .runtime_watch_cancel
        .cancel();
    // The worker accepts a switch the daemon never imports before the stop.
    rig.report("conversation-before-stop").await;

    rig.registry.stop(rig.id()).await.expect("stop");

    let info = rig.registry.inspect(rig.id()).await.expect("inspect");
    assert_eq!(
        info.native_session_id.as_deref(),
        Some("conversation-before-stop")
    );
    let record = rig.durable_record();
    assert_eq!(
        record
            .recovery
            .expect("recovery")
            .native_session_id
            .as_deref(),
        Some("conversation-before-stop")
    );
}

#[tokio::test]
async fn a_recovered_generations_first_worker_report_numbered_zero_is_imported() {
    let rig = Rig::new(
        "supersede-recovered-zero",
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .await;
    rig.report("conversation-before-recovery").await;
    assert_eq!(
        rig.settle(&rig.registry).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.exit(&rig.registry).await;
    rig.registry.resume(rig.id()).await.expect("resume");
    wait_for_file_contains(&rig.marker, "--session\n").await;

    let mut snapshot = rig.snapshot(&rig.registry).await;
    let root = snapshot.child_process.expect("worker child");
    snapshot.launch_identity = Some(pohunek_worker_protocol::ReportedLaunchIdentity {
        provider: RUNTIME.to_owned(),
        process: root,
        reference_kind: "id".to_owned(),
        native_reference: "conversation-before-recovery".to_owned(),
    });
    snapshot.native_reference = Some(pohunek_worker_protocol::ReportedNativeReference {
        provider: RUNTIME.to_owned(),
        process: root,
        sequence: 0,
        reference_kind: "id".to_owned(),
        native_reference: "first-report-of-the-generation".to_owned(),
    });
    assert_eq!(
        rig.settle_with(&rig.registry, &snapshot).await,
        WorkerMetadataApplyOutcome::Applied
    );
    rig.assert_held(
        &rig.registry,
        "first-report-of-the-generation",
        NativeReferenceProvenance::Reported,
    )
    .await;
    rig.finish(&rig.registry).await;
}
