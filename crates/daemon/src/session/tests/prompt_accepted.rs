//! Prompt-submit hook evidence across a real worker, the real hook asset and
//! the daemon's bounded input waits.
//!
//! The agent is a script run by a copy of `bash` named `claude`, so the worker
//! accepts it as the launch process. It runs the shipped Claude state hook for
//! every line the daemon types, as the real `UserPromptSubmit` hook would, and
//! it never draws a working frame, so a satisfied wait can only come from the
//! hook report. Expected outcomes follow the delivery-evidence contract
//! (#1486): an acceptance answers a `working` wait of an opted-in caller and is
//! never activity, a revision, an event or a turn.

// Rust guideline compliant 2026-10-23

use super::*;
use crate::agent::host::{
    BuiltinSource, DefinitionError, RuntimeRegistry, RuntimeSource, SourceTrust,
};
use crate::detect::generic_shell_manifest;
use pohunek_worker_protocol::{InputFragment, SecretBytes};
use protocol::PackageDigest;

/// Stands in for the conversation reference the launch process reports.
const NATIVE_ID: &str = "fixture-claude-conversation";

/// Text typed by the waited inputs; it holds no activity keyword.
const TYPED: &str = "hello";

/// The longest wait deadline: long next to the hook's run time on a loaded host.
const PATIENT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;

/// A deadline that elapses on purpose: the unacknowledged outcome is the
/// scenario input, and a real PTY agent offers no paused clock.
const IMPATIENT_MS: u32 = 600;

/// A test source serving Claude as an official package, in place of the builtin.
#[derive(Debug)]
struct ClaudeSource(RuntimeDefinition);

impl RuntimeSource for ClaudeSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Official
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(vec![self.0.clone()])
    }
}

/// Builds a Claude hook runtime whose executable is selected by this test.
///
/// A non-zero `submit_delay_ms` gives the delayed-submit framing of the real
/// package; zero gives a single write plan.
fn claude_host(program: &Path, script: &Path, submit_delay_ms: u32) -> RuntimeHost {
    let document = format!(
        r#"schema = 1
id = "pohunek.runtime.claude"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "claude"
name = "Fixture Claude"
program = {program:?}
args = [{script:?}]
detect_manifest = "claude"
prompt_arg = false

[input]
bracketed_paste = false
submit_delay_ms = {submit_delay_ms}
submit_delay_configurable = false
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--resume", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "hook"

[integration]
handler = "claude-hook-v1"
hook_schema = "identity-subagent-v1"

[config_home]
env = "CLAUDE_CONFIG_DIR"
default = ".claude"
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
    .expect("fixture Claude descriptor");
    let builtin = BuiltinSource::new("/bin/sh");
    let official = ClaudeSource(definition);
    RuntimeHost::new(RuntimeRegistry::from_sources(&[&builtin, &official]).expect("runtime host"))
}

/// A launch backed by a real worker and the shipped Claude state hook.
struct Rig {
    registry: SessionRegistry,
    dir: PathBuf,
    /// Lines the agent appends as it works: `ready`, `skipped`, `hook-done`.
    state: PathBuf,
    /// Present: the agent reads a line and reports nothing.
    skip_flag: PathBuf,
    /// Present: the agent parks on `hold_gate` before it runs the hook.
    hold_flag: PathBuf,
    /// Present: the agent prints output before it runs the hook, which the
    /// detector reads as work.
    noisy_flag: PathBuf,
    /// Present: the hook runs in a child shell of the agent, as the hook of a
    /// nested `claude -p` run by the agent would.
    nested_flag: PathBuf,
    /// Present: the agent reports nothing for the line `b`.
    skip_b_flag: PathBuf,
    hold_gate: fs::File,
    session: Option<SessionInfo>,
}

impl Rig {
    /// Prepares the registry; [`Self::start`] creates the session.
    fn new(tag: &str, submit_delay_ms: u32) -> Self {
        let dir = temp_dir(tag);
        let state = dir.join("agent.state");
        let skip_flag = dir.join("skip");
        let hold_flag = dir.join("hold");
        let noisy_flag = dir.join("noisy");
        let nested_flag = dir.join("nested");
        let skip_b_flag = dir.join("skip-b");
        let hold_gate_path = dir.join("hold.gate");
        let hold_gate = hook_gate(&hold_gate_path);
        let script = dir.join("claude-agent.sh");
        let adapter = crate::integration::runnable_script("claude", "pohunek-agent-state.sh");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\n\
                 stty -echo\n\
                 printf '{{\"session_id\":\"{NATIVE_ID}\"}}' | sh '{adapter}' session\n\
                 printf 'ready\\n' >> '{state}'\n\
                 while IFS= read -r line; do\n\
                   if [ -e '{skip_b}' ] && [ \"$line\" = b ]; then printf 'skipped\\n' >> '{state}'; continue; fi\n\
                   if [ -e '{skip}' ]; then printf 'skipped\\n' >> '{state}'; continue; fi\n\
                   if [ -e '{noisy}' ]; then printf 'streaming output\\n'; fi\n\
                   if [ -e '{hold}' ]; then printf 'parked\\n' >> '{parked}'; read -r gate < '{gate}'; fi\n\
                   if [ -e '{nested}' ]; then\n\
                     ( printf '{{\"prompt\":\"typed\"}}' | sh '{adapter}' prompt-submit )\n\
                   else\n\
                     printf '{{\"prompt\":\"typed\"}}' | sh '{adapter}' prompt-submit\n\
                   fi\n\
                   printf 'hook-done\\n' >> '{state}'\n\
                 done\n",
                adapter = adapter.display(),
                state = state.display(),
                skip = skip_flag.display(),
                skip_b = skip_b_flag.display(),
                hold = hold_flag.display(),
                parked = dir.join("hold.parked").display(),
                noisy = noisy_flag.display(),
                nested = nested_flag.display(),
                gate = hold_gate_path.display(),
            ),
        );
        let program = dir.join("claude");
        pohunek_test_support::fs::copy_shell_image(native_supersede::bash_path(), &program)
            .expect("copy the fixture agent image");
        let host = claude_host(&program, &script, submit_delay_ms);
        let mut config = SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(temp_store_path(tag)),
            ..SessionRegistryConfig::default()
        };
        let (runtime_root, state_root) = test_worker_roots(&config);
        config.supervision = Some(crate::session::test_supervision(&runtime_root, &state_root));
        config.worker_runtime_root = Some(runtime_root.clone());
        config.worker_state_root = Some(state_root.clone());
        let launcher: Arc<dyn crate::runtime::WorkerLauncher> = Arc::new(
            crate::runtime::InProcessWorkerLauncher::new(runtime_root, state_root),
        );
        let registry = SessionRegistry::new_with_runtimes_and_launcher(
            config,
            host,
            launcher,
            Arc::new(ReadableHost::new()),
        );
        Self {
            registry,
            dir,
            state,
            skip_flag,
            hold_flag,
            noisy_flag,
            nested_flag,
            skip_b_flag,
            hold_gate,
            session: None,
        }
    }

    fn params(&self) -> SessionNewParams {
        SessionNewParams {
            agent: "claude".to_owned(),
            cwd: Some(self.dir.clone()),
            ..params()
        }
    }

    /// Creates the session and waits until the agent is the launch process.
    async fn start(tag: &str, submit_delay_ms: u32) -> Self {
        let mut rig = Self::new(tag, submit_delay_ms);
        let session = rig
            .registry
            .create(rig.params())
            .await
            .expect("create the Claude session");
        rig.session = Some(session);
        rig.wait_launch().await;
        rig
    }

    /// Waits until the worker accepted the launch identity the agent reported.
    async fn wait_launch(&self) {
        wait_for_file_contains(&self.state, "ready").await;
        let (worker, _) = live_worker_and_identity(&self.registry, self.id()).await;
        wait_until("the launch identity", || async {
            worker
                .inspect()
                .await
                .ok()
                .filter(|snapshot| snapshot.launch_identity.is_some())
        })
        .await;
    }

    fn id(&self) -> &SessionId {
        &self.session.as_ref().expect("a started session").id
    }

    /// How many times the agent has finished running the prompt-submit hook.
    fn hook_runs(&self) -> usize {
        fs::read_to_string(&self.state)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "hook-done")
            .count()
    }

    async fn wait_hook_runs(&self, count: usize) {
        wait_until("the prompt-submit hook to finish", || async {
            (self.hook_runs() >= count).then_some(())
        })
        .await;
    }

    /// Waits until the daemon imported `count` acceptances for the session.
    async fn wait_imported(&self, count: usize) {
        wait_until("the daemon to import the acceptance", || async {
            let sessions = self.registry.inner.sessions.lock().await;
            (sessions.get(self.id())?.prompt_acceptances.len() >= count).then_some(())
        })
        .await;
    }

    fn input(
        &self,
        until: Vec<AgentActivity>,
        timeout_ms: u32,
        allow_prompt_accepted: Option<bool>,
    ) -> tokio::task::JoinHandle<Result<protocol::SessionInputResult, protocol::ProtocolError>>
    {
        self.input_text(TYPED, until, timeout_ms, allow_prompt_accepted)
    }

    fn input_text(
        &self,
        text: &str,
        until: Vec<AgentActivity>,
        timeout_ms: u32,
        allow_prompt_accepted: Option<bool>,
    ) -> tokio::task::JoinHandle<Result<protocol::SessionInputResult, protocol::ProtocolError>>
    {
        let registry = self.registry.clone();
        let params = protocol::SessionInputParams {
            session_id: self.id().clone(),
            text: text.to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(until),
                timeout_ms: Some(timeout_ms),
                allow_prompt_accepted,
            }),
        };
        tokio::spawn(async move { registry.input(params).await })
    }

    /// Types a line into the agent's terminal as the agent's own turn would
    /// arrive: straight to the worker, so the daemon records no input.
    async fn self_start(&self) {
        let (worker, _) = live_worker_and_identity(&self.registry, self.id()).await;
        worker
            .write(vec![InputFragment {
                bytes: SecretBytes::new(b"self-started\r".to_vec()),
                delay_after_ms: 0,
            }])
            .await
            .expect("type into the agent");
    }

    /// Makes the session read as `working` at the next input boundary.
    async fn mark_working(&self) {
        let mut sessions = self.registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(self.id()).expect("live session");
        entry.info.activity = Some(AgentActivity::Working);
    }

    /// Makes the session read as `idle` at the next input boundary.
    async fn mark_idle(&self) {
        let mut sessions = self.registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(self.id()).expect("live session");
        entry.info.activity = Some(AgentActivity::Idle);
    }

    /// Waits until the session holds `working` activity evidence.
    async fn wait_working_evidence(&self) {
        wait_until("working evidence", || async {
            let sessions = self.registry.inner.sessions.lock().await;
            sessions
                .get(self.id())?
                .activity_evidence
                .iter()
                .any(|evidence| evidence.activity == AgentActivity::Working)
                .then_some(())
        })
        .await;
    }

    async fn finish(&self) {
        let _ = self.registry.stop(self.id()).await;
    }
}

#[tokio::test]
async fn a_hook_only_acceptance_satisfies_a_working_wait_without_inventing_activity() {
    let rig = Rig::start("prompt-hook-only", 0).await;
    let before = rig.registry.inspect(rig.id()).await.expect("inspect");

    let result = rig
        .input(vec![AgentActivity::Working], PATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect("the acceptance answers the wait");

    assert!(result.accepted);
    assert_eq!(result.submitted, Some(true));
    assert_eq!(result.prompt_accepted, Some(true));
    assert_eq!(result.activity, None, "no working is fabricated");
    assert_eq!(result.activity_source, None);
    assert_eq!(result.activity_revision, None);
    assert_eq!(
        result.runtime.as_ref(),
        before
            .runtime
            .as_ref()
            .map(|runtime| {
                protocol::SessionRuntimeIdentity::new(
                    runtime.worker_instance_id.clone().expect("instance"),
                    runtime.runtime_generation,
                )
                .expect("identity")
            })
            .as_ref()
    );
    assert_eq!(
        result.activity_epoch.as_deref(),
        Some(rig.registry.daemon_instance_id())
    );
    rig.finish().await;
}

#[tokio::test]
async fn an_acceptance_without_input_is_never_activity_a_revision_an_event_or_a_turn() {
    let rig = Rig::start("prompt-no-input", 0).await;
    let mut events = rig.registry.subscribe();
    let revision = rig
        .registry
        .input_activity_snapshot(rig.id(), None)
        .await
        .expect("snapshot")
        .revision;
    let turns = rig.registry.inner.turns.latest();
    drain_agent_state(&mut events);

    rig.self_start().await;
    rig.wait_hook_runs(1).await;
    rig.wait_imported(1).await;

    assert_eq!(
        rig.registry
            .input_activity_snapshot(rig.id(), None)
            .await
            .expect("snapshot")
            .revision,
        revision,
        "the activity revision did not move"
    );
    assert_eq!(drain_agent_state(&mut events), 0, "no agent_state event");
    assert_eq!(rig.registry.inner.turns.latest(), turns, "no turn edge");

    // A later turn of the agent's own, with output the detector reads as work,
    // opens no turn either: no input armed it.
    fs::write(&rig.noisy_flag, b"").expect("make the agent print output");
    rig.self_start().await;
    rig.wait_hook_runs(2).await;
    rig.wait_imported(2).await;
    rig.wait_working_evidence().await;
    assert_eq!(rig.registry.inner.turns.latest(), turns, "no turn edge");
    fs::remove_file(&rig.noisy_flag).expect("stop the output");
    rig.mark_idle().await;

    // The self-started records are older than the next Enter, so a waited input
    // whose own hook never reports does not match them.
    fs::write(&rig.skip_flag, b"").expect("lose the next hook");
    let error = rig
        .input(vec![AgentActivity::Working], IMPATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect_err("an earlier acceptance answers no later input");
    assert_eq!(error.code, "session_input_timeout");
    rig.finish().await;
}

#[tokio::test]
async fn without_the_opt_in_an_acceptance_answers_nothing() {
    let rig = Rig::start("prompt-no-opt-in", 0).await;

    let error = rig
        .input(vec![AgentActivity::Working], IMPATIENT_MS, None)
        .await
        .expect("wait task joins")
        .expect_err("an older client never gets a hook-only success");

    assert_eq!(error.code, "session_input_timeout");
    assert!(
        !error.msg.contains("prompt acceptance"),
        "the message of a caller that never asked for acceptance: {}",
        error.msg
    );
    rig.wait_hook_runs(1).await;
    rig.wait_imported(1).await;
    rig.finish().await;
}

#[tokio::test]
async fn a_lost_or_late_hook_ends_in_a_timeout_that_names_both_kinds_of_evidence() {
    let rig = Rig::start("prompt-lost-late", 0).await;

    fs::write(&rig.skip_flag, b"").expect("lose the hook");
    let lost = rig
        .input(vec![AgentActivity::Working], IMPATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect_err("a lost hook never satisfies the wait");
    assert_eq!(lost.code, "session_input_timeout");
    assert!(
        lost.msg
            .contains("neither the requested activity nor a prompt acceptance"),
        "{}",
        lost.msg
    );
    fs::remove_file(&rig.skip_flag).expect("restore the hook");

    fs::write(&rig.hold_flag, b"").expect("hold the hook");
    let late = rig
        .input(vec![AgentActivity::Working], IMPATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect_err("an acceptance after the deadline does not count");
    assert_eq!(late.code, "session_input_timeout");
    fs::remove_file(&rig.hold_flag).expect("stop holding");
    let mut gate = &rig.hold_gate;
    std::io::Write::write_all(&mut gate, b"release\n").expect("release the parked hook");
    rig.wait_hook_runs(1).await;
    rig.finish().await;
}

#[tokio::test]
async fn a_busy_agent_at_the_boundary_does_not_use_the_acceptance() {
    let rig = Rig::start("prompt-busy-boundary", 0).await;
    rig.mark_working().await;

    let error = rig
        .input(vec![AgentActivity::Working], IMPATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect_err("a queued input is not acknowledged by the hook");

    assert_eq!(error.code, "session_input_timeout");
    assert!(
        !error.msg.contains("prompt acceptance"),
        "the acceptance was not eligible: {}",
        error.msg
    );
    rig.wait_hook_runs(1).await;
    rig.wait_imported(1).await;
    rig.finish().await;
}

#[tokio::test]
async fn working_evidence_and_an_acceptance_are_reported_together() {
    let rig = Rig::start("prompt-both", 0).await;
    fs::write(&rig.noisy_flag, b"").expect("make the agent print output");
    let sent = Arc::new(tokio::sync::Barrier::new(2));
    rig.registry.hold_next_input_sent(Arc::clone(&sent));
    let waiting = rig.input(vec![AgentActivity::Working], PATIENT_MS, Some(true));
    reach_boundary(&sent).await;
    rig.wait_imported(1).await;
    rig.wait_working_evidence().await;
    reach_boundary(&sent).await;

    let result = waiting
        .await
        .expect("wait task joins")
        .expect("both kinds of evidence are present");

    assert_eq!(result.activity, Some(AgentActivity::Working));
    assert!(result.activity_source.is_some(), "{result:?}");
    assert_eq!(result.prompt_accepted, Some(true));
    rig.finish().await;
}

#[tokio::test]
async fn an_idle_wait_after_delayed_submit_is_not_answered_by_an_acceptance() {
    let rig = Rig::start("prompt-idle-delayed", 150).await;

    let error = rig
        .input(vec![AgentActivity::Idle], IMPATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect_err("idle still needs working evidence after Enter");

    assert_eq!(error.code, "session_input_timeout");
    rig.wait_hook_runs(1).await;
    rig.wait_imported(1).await;
    rig.finish().await;
}

#[tokio::test]
async fn a_hook_only_acceptance_also_answers_a_delayed_submit_wait() {
    let rig = Rig::start("prompt-working-delayed", 150).await;

    let result = rig
        .input(vec![AgentActivity::Working], PATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect("the acceptance answers the wait");

    assert_eq!(result.prompt_accepted, Some(true));
    assert_eq!(result.activity, None);
    rig.finish().await;
}

#[tokio::test]
async fn session_new_reports_prompt_delivered_from_the_acceptance_or_not_at_all() {
    for (tag, lose_hook, expected) in [
        ("prompt-new-accepted", false, Some(true)),
        ("prompt-new-lost", true, Some(false)),
    ] {
        let mut rig = Rig::new(tag, 0);
        if lose_hook {
            fs::write(&rig.skip_flag, b"").expect("lose the hook");
        }
        arm_ready_mark(&rig.registry);
        let mut create = rig.params();
        create.input = Some(TYPED.to_owned());
        create.input_ack_timeout_ms = Some(if lose_hook { IMPATIENT_MS } else { PATIENT_MS });
        let outcome = create_acknowledged(&rig.registry, create)
            .await
            .expect("the create keeps its session");
        assert_eq!(outcome.1, expected, "{tag}");
        rig.session = Some(outcome.0);
        rig.finish().await;
    }
}

#[tokio::test]
async fn a_report_from_a_nested_agent_process_is_never_an_acceptance() {
    let rig = Rig::start("prompt-nested", 0).await;
    fs::write(&rig.nested_flag, b"").expect("run the hook in a child process");

    let error = rig
        .input(vec![AgentActivity::Working], IMPATIENT_MS, Some(true))
        .await
        .expect("wait task joins")
        .expect_err("a hook of another process is not the launch process's");

    assert_eq!(error.code, "session_input_timeout");
    rig.wait_hook_runs(1).await;
    let (worker, _) = live_worker_and_identity(&rig.registry, rig.id()).await;
    assert_eq!(
        worker
            .inspect()
            .await
            .expect("inspect")
            .prompt_accepted_sequence,
        None,
        "the worker applied nothing"
    );
    rig.finish().await;
}

#[tokio::test]
async fn an_acceptance_after_the_runtime_ended_answers_nothing() {
    let rig = Rig::start("prompt-runtime-ended", 0).await;
    fs::write(&rig.hold_flag, b"").expect("hold the hook");
    let waiting = rig.input(vec![AgentActivity::Working], PATIENT_MS, Some(true));
    wait_for_file_contains(&rig.dir.join("hold.parked"), "parked").await;

    rig.registry.stop(rig.id()).await.expect("stop the session");
    let error = waiting
        .await
        .expect("wait task joins")
        .expect_err("a hook report cannot outlive the runtime it was meant for");

    assert_ne!(error.code, "session_input_timeout", "{error}");
    assert!(
        matches!(
            error.code.as_str(),
            "session_not_running" | "session_runtime_changed" | "session_runtime_lost"
        ),
        "{error}"
    );
}

#[tokio::test]
async fn an_acceptance_between_the_body_and_enter_is_not_this_submissions() {
    let rig = Rig::start("prompt-between-plans", 150).await;
    // The body ends a line, so the agent reports it while Enter is still to be
    // written; Enter's own line `b` reports nothing.
    fs::write(&rig.skip_b_flag, b"").expect("lose the hook of Enter's line");
    let parked = Arc::new(tokio::sync::Barrier::new(2));
    rig.registry.hold_next_submit_delay(Arc::clone(&parked));
    let waiting = rig.input_text("a\nb", vec![AgentActivity::Working], PATIENT_MS, Some(true));
    reach_boundary(&parked).await;
    rig.wait_hook_runs(1).await;
    reach_boundary(&parked).await;

    // The daemon reads the worker only once the submit released its control
    // connection, so the report is imported after Enter's mark was taken.
    let error = waiting
        .await
        .expect("wait task joins")
        .expect_err("an acceptance older than Enter's mark answers nothing");
    assert_eq!(error.code, "session_input_timeout");
    rig.finish().await;
}

#[tokio::test]
async fn an_acceptance_never_arms_a_turn_for_output_the_agent_prints_on_its_own() {
    let rig = Rig::start("prompt-no-turn", 0).await;
    rig.registry
        .turn_events()
        .flap_window_handle()
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let mut edges = rig
        .registry
        .turn_events()
        .take_edge_receiver()
        .expect("the first taker");

    // The agent's own prompt reaches the hook and is imported; a later prompt
    // of its own makes it print output the detector reads as work, and it
    // finishes. No input armed a turn, so the session opens and ends none.
    rig.self_start().await;
    rig.wait_hook_runs(1).await;
    rig.wait_imported(1).await;
    fs::write(&rig.noisy_flag, b"").expect("make the agent print output");
    rig.self_start().await;
    rig.wait_hook_runs(2).await;
    rig.wait_working_evidence().await;
    fs::remove_file(&rig.noisy_flag).expect("stop the output");
    rig.registry
        .record_activity(rig.id(), transition(AgentActivity::Idle))
        .await;

    assert_eq!(started_turns_and_ends(&mut edges), (0, 0));
    rig.finish().await;
}

#[tokio::test]
async fn a_session_of_another_runtime_gets_neither_the_field_nor_the_acceptance_wording() {
    let rig = Rig::new("prompt-other-runtime", 0);
    let session = shell_session(&rig.registry).await;
    let wait = |timeout_ms| protocol::SessionInputParams {
        session_id: session.id.clone(),
        text: TYPED.to_owned(),
        wait: Some(protocol::SessionInputWait {
            until: Some(vec![AgentActivity::Working]),
            timeout_ms: Some(timeout_ms),
            allow_prompt_accepted: Some(true),
        }),
    };

    // The shell's own activity may answer the wait or not; either way no
    // acceptance is involved.
    match rig.registry.input(wait(IMPATIENT_MS)).await {
        Ok(result) => assert_eq!(
            result.prompt_accepted, None,
            "the runtime has no such field"
        ),
        Err(error) => {
            assert_eq!(error.code, "session_input_timeout");
            assert!(
                !error.msg.contains("prompt acceptance"),
                "the message names evidence the runtime cannot produce: {}",
                error.msg
            );
        }
    }

    let task = {
        let registry = rig.registry.clone();
        let params = wait(PATIENT_MS);
        tokio::spawn(async move { registry.input(params).await })
    };
    // Each probe reports `working` then `idle`; the first report above the
    // input's boundary answers the wait.
    wait_until("the shell wait to be answered", || async {
        if task.is_finished() {
            return Some(());
        }
        rig.registry
            .record_activity(&session.id, transition(AgentActivity::Working))
            .await;
        rig.registry
            .record_activity(&session.id, transition(AgentActivity::Idle))
            .await;
        None
    })
    .await;
    let result = task
        .await
        .expect("wait task joins")
        .expect("activity answers the wait");
    assert_eq!(result.activity, Some(AgentActivity::Working));
    assert_eq!(
        result.prompt_accepted, None,
        "the runtime has no such field"
    );
    let _ = rig.registry.stop(&session.id).await;
}
