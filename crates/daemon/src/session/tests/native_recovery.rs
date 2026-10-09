//! Codex hook recovery across a real worker journal and durable daemon store.

// Rust guideline compliant 2026-10-09

use super::*;
use crate::agent::host::{
    BuiltinSource, DefinitionError, RuntimeRegistry, RuntimeSource, SourceTrust,
};
use crate::detect::generic_shell_manifest;
use protocol::PackageDigest;

/// Synthetic conversation reference that cannot name an operator conversation.
const NATIVE_ID: &str = "fixture-codex-conversation";
/// A later conversation selected inside the same verified Codex process.
const SWITCHED_ID: &str = "fixture-codex-switched";

/// A test source serving Codex as an official package, in place of the builtin.
#[derive(Debug)]
struct CodexSource(RuntimeDefinition);

impl RuntimeSource for CodexSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Official
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(vec![self.0.clone()])
    }
}

/// Builds a Codex hook runtime whose executable is selected by this test.
fn codex_host(program: &std::path::Path, script: &std::path::Path) -> RuntimeHost {
    let document = format!(
        r#"schema = 1
id = "pohunek.runtime.codex"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "codex"
name = "Fixture Codex"
program = {program:?}
args = [{script:?}]
detect_manifest = "codex"
prompt_arg = true

[input]
bracketed_paste = true
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["resume", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "hook"

[integration]
handler = "codex-hook-v1"
hook_schema = "identity-subagent-v1"

[config_home]
env = "CODEX_HOME"
default = ".codex"
"#
    );
    let definition = RuntimeDefinition::from_toml(
        &document,
        |package| DefinitionOrigin::Package {
            package,
            digest: PackageDigest::parse(crate::agent::host::fixture::PACKAGE_DIGEST)
                .expect("fixture digest"),
        },
        |_name| Ok(Arc::new(generic_shell_manifest().clone())),
    )
    .expect("fixture Codex descriptor");
    let builtin = BuiltinSource::new("/bin/sh");
    let official = CodexSource(definition);
    RuntimeHost::new(RuntimeRegistry::from_sources(&[&builtin, &official]).expect("runtime host"))
}

/// A launch backed by a real worker, official-shaped Codex hook and store.
struct Rig {
    registry: SessionRegistry,
    host: RuntimeHost,
    launcher: Arc<dyn crate::runtime::WorkerLauncher>,
    config: SessionRegistryConfig,
    session: SessionInfo,
    commands: fs::File,
    ack: PathBuf,
    launches: PathBuf,
    store_path: PathBuf,
}

impl Rig {
    async fn new(tag: &str, verified_launch: bool) -> Self {
        let dir = temp_dir(tag);
        let commands_path = dir.join("commands.fifo");
        let commands = hook_gate(&commands_path);
        let ack = dir.join("hook.ack");
        let launch_log_path = dir.join("launches.txt");
        let script = dir.join("codex-agent.sh");
        let adapter = crate::integration::runnable_script("codex", "pohunek-agent-state.sh");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{launches}'\n\
                 while read -r command reference < '{commands_path}'; do\n\
                   case \"$command\" in\n\
                     report) printf '{{\"session_id\":\"%s\"}}' \"$reference\" | sh '{adapter}' session; printf '%s\\n' \"$reference\" >> '{ack}' ;;\n\
                     exit) exit 0 ;;\n\
                   esac\n\
                 done\n",
                launches = launch_log_path.display(),
                commands_path = commands_path.display(),
                adapter = adapter.display(),
                ack = ack.display(),
            ),
        );
        let program = if verified_launch {
            let copied = dir.join("codex");
            fs::copy(native_supersede::bash_path(), &copied).expect("copy fixture agent image");
            copied
        } else {
            native_supersede::bash_path()
        };
        let host = codex_host(&program, &script);
        let store_path = temp_store_path(tag);
        let mut config = SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        };
        let (runtime_root, state_root) = test_worker_roots(&config);
        config.supervision = Some(crate::session::test_supervision(&runtime_root, &state_root));
        config.worker_runtime_root = Some(runtime_root.clone());
        config.worker_state_root = Some(state_root.clone());
        let launcher: Arc<dyn crate::runtime::WorkerLauncher> = Arc::new(
            crate::runtime::InProcessWorkerLauncher::new(runtime_root, state_root),
        );
        let registry = Self::registry(&config, &host, &launcher);
        let session = registry
            .create(SessionNewParams {
                agent: "codex".to_owned(),
                cwd: Some(dir),
                ..params()
            })
            .await
            .expect("create Codex session");
        assert_eq!(
            session.native_session_id, None,
            "hook runtime starts unbound"
        );
        Self {
            registry,
            host,
            launcher,
            config,
            session,
            commands,
            ack,
            launches: launch_log_path,
            store_path,
        }
    }

    fn registry(
        config: &SessionRegistryConfig,
        host: &RuntimeHost,
        launcher: &Arc<dyn crate::runtime::WorkerLauncher>,
    ) -> SessionRegistry {
        SessionRegistry::new_with_runtimes_and_launcher(
            config.clone(),
            host.clone(),
            Arc::clone(launcher),
            Arc::new(ReadableHost::new()),
        )
    }

    async fn report(&self, reference: &str) -> pohunek_worker_protocol::InspectSnapshot {
        let mut writer = &self.commands;
        writer
            .write_all(format!("report {reference}\n").as_bytes())
            .expect("signal fixture agent");
        wait_for_file_contains(&self.ack, reference).await;
        let (worker, _) = live_worker_and_identity(&self.registry, &self.session.id).await;
        wait_until(
            "Codex hook identity to enter the worker journal",
            || async {
                let snapshot = worker.inspect().await.ok()?;
                let matches = snapshot
                    .active_identity
                    .as_ref()
                    .and_then(|identity| identity.native_reference.as_deref())
                    == Some(reference);
                matches.then_some(snapshot)
            },
        )
        .await
    }

    async fn settle(&self, snapshot: &pohunek_worker_protocol::InspectSnapshot) {
        let outcome = wait_until("daemon to settle Codex worker identity", || async {
            match Box::pin(
                self.registry
                    .apply_worker_metadata_snapshot(&self.session.id, snapshot),
            )
            .await
            {
                WorkerMetadataApplyOutcome::Retryable(_) => None,
                settled => Some(settled),
            }
        })
        .await;
        assert_eq!(outcome, WorkerMetadataApplyOutcome::Applied);
    }

    fn durable_record(&self) -> crate::store::SessionRecord {
        crate::store::Store::new(self.store_path.clone())
            .load_sessions()
            .expect("load session store")
            .into_iter()
            .find(|record| record.session_id == self.session.id.0)
            .expect("durable Codex record")
    }

    async fn detach_daemon(&mut self) {
        let barrier = self.registry.inner.persist_lock.lock().await;
        std::mem::forget(barrier);
        let entry = self
            .registry
            .inner
            .sessions
            .lock()
            .await
            .remove(&self.session.id)
            .expect("live session");
        entry.cancel_runtime_watchers();
        drop(entry);
    }

    async fn adopt_daemon(&mut self) {
        let next = Self::registry(&self.config, &self.host, &self.launcher);
        next.reconcile_workers()
            .await
            .expect("adopt live Codex worker");
        self.registry = next;
    }

    async fn restart(&mut self) {
        self.detach_daemon().await;
        self.adopt_daemon().await;
    }

    async fn finish(&self) {
        let _ = self.registry.stop(&self.session.id).await;
    }

    async fn remove(&self) {
        self.registry
            .remove(&self.session.id)
            .await
            .expect("retire fixture worker");
    }

    async fn refused_resume_code(&self) -> String {
        let before = self
            .registry
            .inspect(&self.session.id)
            .await
            .expect("stopped fixture session");
        let launches = fs::read_to_string(&self.launches).expect("initial launch marker");
        let error = self
            .registry
            .resume(&self.session.id)
            .await
            .expect_err("untrusted Codex identity cannot resume");
        let after = self
            .registry
            .inspect(&self.session.id)
            .await
            .expect("session remains after refusal");
        self.remove().await;
        assert_eq!(
            after
                .runtime
                .as_ref()
                .map(|runtime| runtime.runtime_generation),
            before
                .runtime
                .as_ref()
                .map(|runtime| runtime.runtime_generation),
            "refused recovery must not create a runtime generation"
        );
        assert_eq!(
            fs::read_to_string(&self.launches).expect("launch marker after refusal"),
            launches,
            "refused recovery must not launch another agent"
        );
        error.code
    }
}

#[tokio::test]
async fn a_missing_codex_hook_claim_reports_native_identity_missing() {
    let rig = Rig::new("codex-no-native-report", true).await;
    let (worker, _) = live_worker_and_identity(&rig.registry, &rig.session.id).await;
    let snapshot = worker.inspect().await.expect("worker snapshot");
    assert!(snapshot.active_identity.is_none());
    assert!(snapshot.launch_identity.is_none());
    assert!(snapshot.native_reference.is_none());
    rig.finish().await;

    assert_eq!(rig.refused_resume_code().await, "native_identity_missing");
}

#[tokio::test]
async fn an_unreadable_codex_generation_journal_is_not_classified_as_a_missing_claim() {
    let rig = Rig::new("codex-unreadable-native", true).await;
    rig.finish().await;
    let record = rig.durable_record();
    let journal = rig
        .config
        .worker_state_root
        .as_ref()
        .expect("worker state root")
        .join(&rig.session.id.0)
        .join(format!(
            "{}.json",
            record.runtime.worker_id.expect("recorded worker")
        ));
    let original = fs::read(&journal).expect("terminal worker journal");
    fs::write(&journal, b"invalid journal").expect("corrupt fixture journal");

    let error = rig
        .registry
        .resume(&rig.session.id)
        .await
        .expect_err("journal cannot prove absence");
    assert_eq!(error.code, "native_identity_evidence_unavailable");

    fs::write(&journal, original).expect("restore fixture journal");
    rig.remove().await;
}

#[tokio::test]
async fn a_codex_journal_identity_without_a_verified_launch_stays_unbound() {
    let rig = Rig::new("codex-unverified-native", false).await;
    let snapshot = rig.report(NATIVE_ID).await;
    assert!(
        snapshot.launch_identity.is_none(),
        "launch remains unverified"
    );
    assert!(
        snapshot.native_reference.is_none(),
        "no launch reference promoted"
    );
    rig.settle(&snapshot).await;

    let current = rig
        .registry
        .inspect(&rig.session.id)
        .await
        .expect("inspect");
    assert_eq!(current.active_agent_session_id.as_deref(), Some(NATIVE_ID));
    assert_eq!(current.native_session_id, None);
    let durable = rig.durable_record();
    assert_eq!(durable.info.native_session_id, None);
    assert_eq!(
        durable
            .recovery
            .expect("recovery binding")
            .native_session_id,
        None,
    );
    rig.finish().await;
    assert_eq!(
        rig.refused_resume_code().await,
        "native_identity_unverified"
    );
}

#[tokio::test]
async fn a_verified_codex_launch_identity_survives_daemon_restart_and_resume() {
    let mut rig = Rig::new("codex-verified-native", true).await;
    let snapshot = rig.report(NATIVE_ID).await;
    assert!(
        snapshot.launch_identity.is_some(),
        "launch process verified"
    );
    assert!(
        snapshot.native_reference.is_some(),
        "worker journals native reference"
    );
    rig.settle(&snapshot).await;
    assert_eq!(
        rig.durable_record().info.native_session_id.as_deref(),
        Some(NATIVE_ID)
    );

    rig.restart().await;
    let adopted = rig
        .registry
        .inspect(&rig.session.id)
        .await
        .expect("adopted session");
    assert_eq!(adopted.native_session_id.as_deref(), Some(NATIVE_ID));
    rig.finish().await;
    rig.registry
        .resume(&rig.session.id)
        .await
        .expect("resume Codex conversation");
    wait_for_file_contains(&rig.launches, &format!("resume {NATIVE_ID}")).await;
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_foreign_codex_reference_is_rejected_with_a_typed_process_reason() {
    let rig = Rig::new("codex-foreign-native", true).await;
    let snapshot = rig.report(NATIVE_ID).await;
    rig.settle(&snapshot).await;
    let mut foreign = snapshot;
    let foreign_process = pohunek_worker_protocol::ProcessIdentity {
        pid: u32::MAX,
        start_identity: 1,
    };
    foreign
        .active_identity
        .as_mut()
        .expect("reported active identity")
        .process = foreign_process;
    let reference = foreign
        .native_reference
        .as_mut()
        .expect("journaled native reference");
    reference.process = foreign_process;
    reference.sequence += 1;
    reference.native_reference = SWITCHED_ID.to_owned();

    let claims = super::super::reconcile::IdentityClaims::from_snapshot(&foreign);
    let failure = super::super::reconcile::validate_identity_claims(&ReadableHost::new(), &claims)
        .expect_err("foreign process cannot claim this PTY");
    assert_eq!(
        failure.reason_and_retryability(),
        ("active_identity_process_invalid", false)
    );
    assert_eq!(
        rig.registry
            .apply_worker_metadata_snapshot(&rig.session.id, &foreign)
            .await,
        WorkerMetadataApplyOutcome::Discarded
    );
    let durable = rig.durable_record();
    rig.finish().await;
    rig.remove().await;
    assert_eq!(durable.info.native_session_id.as_deref(), Some(NATIVE_ID));
    assert_eq!(
        durable
            .recovery
            .as_ref()
            .and_then(|binding| binding.native_session_id.as_deref()),
        Some(NATIVE_ID)
    );
}

#[tokio::test]
async fn a_verified_codex_conversation_switch_must_replace_the_durable_reference() {
    let mut rig = Rig::new("codex-switched-native", true).await;
    let first = rig.report(NATIVE_ID).await;
    assert!(first.launch_identity.is_some());
    rig.settle(&first).await;

    let switched = rig.report(SWITCHED_ID).await;
    assert_eq!(
        switched
            .native_reference
            .as_ref()
            .map(|reference| reference.native_reference.as_str()),
        Some(SWITCHED_ID),
        "the worker journals the launch process's latest conversation"
    );
    rig.settle(&switched).await;
    rig.restart().await;
    let adopted = rig
        .registry
        .inspect(&rig.session.id)
        .await
        .expect("adopted session");
    let durable = rig.durable_record();
    rig.finish().await;
    rig.remove().await;

    assert_eq!(adopted.native_session_id.as_deref(), Some(SWITCHED_ID));
    assert_eq!(durable.info.native_session_id.as_deref(), Some(SWITCHED_ID));
    assert_eq!(
        durable
            .recovery
            .as_ref()
            .and_then(|binding| binding.native_session_id.as_deref()),
        Some(SWITCHED_ID)
    );
}

#[tokio::test]
async fn a_hook_switch_during_daemon_outage_supersedes_the_older_stored_target() {
    let mut rig = Rig::new("codex-switch-during-outage", true).await;
    let first = rig.report(NATIVE_ID).await;
    rig.settle(&first).await;
    assert_eq!(
        rig.durable_record().info.native_session_id.as_deref(),
        Some(NATIVE_ID)
    );
    let (worker, _) = live_worker_and_identity(&rig.registry, &rig.session.id).await;
    rig.detach_daemon().await;

    let mut writer = &rig.commands;
    writer
        .write_all(format!("report {SWITCHED_ID}\n").as_bytes())
        .expect("switch agent conversation while daemon is absent");
    wait_for_file_contains(&rig.ack, SWITCHED_ID).await;
    let switched = wait_until("worker to journal switch during daemon outage", || async {
        let snapshot = worker.inspect().await.ok()?;
        (snapshot
            .native_reference
            .as_ref()
            .is_some_and(|reference| reference.native_reference == SWITCHED_ID))
        .then_some(snapshot)
    })
    .await;
    assert_eq!(
        switched
            .launch_identity
            .as_ref()
            .map(|identity| identity.native_reference.as_str()),
        Some(NATIVE_ID),
        "the immutable launch claim still names the first conversation"
    );
    assert_eq!(
        rig.durable_record().info.native_session_id.as_deref(),
        Some(NATIVE_ID),
        "no daemon observed the switch"
    );
    let mut projected = rig.durable_record();
    super::super::reconcile::import_native_reference(&mut projected, &switched);
    assert_eq!(
        projected.info.native_session_id.as_deref(),
        Some(SWITCHED_ID),
        "the newer report is admissible against the older store"
    );
    drop(worker);

    rig.adopt_daemon().await;
    let adopted = rig
        .registry
        .inspect(&rig.session.id)
        .await
        .expect("adopted");
    let durable = rig.durable_record();
    assert_eq!(adopted.native_session_id.as_deref(), Some(SWITCHED_ID));
    assert_eq!(durable.info.native_session_id.as_deref(), Some(SWITCHED_ID));
    assert_eq!(
        durable
            .recovery
            .as_ref()
            .and_then(|binding| binding.native_session_id.as_deref()),
        Some(SWITCHED_ID)
    );
    rig.finish().await;
    rig.registry
        .resume(&rig.session.id)
        .await
        .expect("resume switched conversation after outage");
    wait_for_file_contains(&rig.launches, &format!("resume {SWITCHED_ID}")).await;
    rig.finish().await;
    rig.remove().await;
}
