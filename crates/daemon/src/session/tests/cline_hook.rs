//! The Cline hook handler across a real worker, the launch-input seam and a
//! fake `cline-bin-<release>` that runs the installed reporter files as direct
//! children with the payloads captured from Cline 3.0.74
//! (`integration/cline/fixtures/PROVENANCE.md`).

// Rust guideline compliant 2026-11-06

use protocol::ProtocolError;

use super::*;
use crate::agent::host::fixture::cline_shaped_host_with;

/// Root session id of the `root_resume` fixture.
const ROOT: &str = "c0ffee00-0000-4000-8000-000000000001";
/// Root session id of the `root_resume_after_clear` fixture.
const AFTER_CLEAR: &str = "0000000000002_00000";
/// Root session id of the `root_resume_after_fork` fixture.
const AFTER_FORK: &str = "0000000000003_00000";
/// Root session id of the `root_start` fixture.
const GENERATED: &str = "0000000000004_00000";
/// Subagent id of the `subagent_resume` fixture.
const SUBAGENT: &str = "agent_00000000-0000-4000-8000-000000000002";
/// Subagent id of the `subagent_resume_escape` and `subagent_shutdown` fixtures.
const ESCAPED_SUBAGENT: &str = "agent_00000000-0000-4000-8000-000000000011";
/// File name of the fake Cline executable; a release binary is named this way.
const CLINE_BIN: &str = "cline-bin-3.0.74";
/// How the fake Cline runs a hook file.
const LOCAL: &str = "local";
/// The environment of a hook run by a shared hub daemon: no sandbox variables.
const HUB: &str = "hub";
/// The environment of a `cline` turn outside any Pohunek session.
const FOREIGN: &str = "foreign";
/// A hook run by a process that has been orphaned out of the PTY's tree.
const DETACHED: &str = "detached";
/// A hook whose worker socket path names no socket.
const NO_SOCKET: &str = "nosocket";
/// A hook without the worker socket variable.
const UNSET_SOCKET: &str = "unsetsocket";
/// A hook without the worker instance variable.
const NO_INSTANCE: &str = "noinstance";
/// A hook without the native reference kind variable.
const NO_KIND: &str = "nokind";

/// A launch backed by a real worker and a fake Cline.
struct Rig {
    registry: SessionRegistry,
    session: SessionInfo,
    commands: fs::File,
    acks: PathBuf,
    hook_runs: PathBuf,
    launches: PathBuf,
    state_root: PathBuf,
    cline_home: PathBuf,
}

impl Rig {
    async fn new(tag: &str) -> Result<Self, ProtocolError> {
        Self::start(tag, &[], &[]).await
    }

    async fn start(
        tag: &str,
        extra_args: &[&str],
        profile_env: &[(&str, &str)],
    ) -> Result<Self, ProtocolError> {
        Self::start_with(tag, extra_args, profile_env, true).await
    }

    async fn start_with(
        tag: &str,
        extra_args: &[&str],
        profile_env: &[(&str, &str)],
        declare_home: bool,
    ) -> Result<Self, ProtocolError> {
        let dir = temp_dir(tag);
        let cline_home = crate::test_support::thread_environment_home().join(".cline");
        fs::create_dir_all(&cline_home).expect("create the Cline config home");
        crate::integration::install_cline(&cline_home).expect("install the reporter");
        let commands = hook_gate(&dir.join("commands.fifo"));
        let acks = dir.join("acks.txt");
        let hook_runs = dir.join("hook-runs.txt");
        let launches = dir.join("launches.txt");
        let script = dir.join("cline-agent.sh");
        write_executable(
            &script,
            &fake_cline(
                &dir.join("commands.fifo"),
                &acks,
                &hook_runs,
                &launches,
                &cline_home.join("hooks"),
                &dir.join("detached.sh"),
            ),
        );
        write_executable(&dir.join("detached.sh"), DETACHED_WRAPPER);
        let program = dir.join(CLINE_BIN);
        pohunek_test_support::fs::copy_shell_image(native_supersede::bash_path(), &program)
            .expect("copy the fixture agent image");
        let args: Vec<String> = std::iter::once(script.display().to_string())
            .chain(extra_args.iter().map(|arg| (*arg).to_owned()))
            .collect();
        let host = cline_shaped_host_with(&program, &args, declare_home);
        let mut config = SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(temp_store_path(tag)),
            ..SessionRegistryConfig::default()
        };
        let mut agent = "cline";
        if !profile_env.is_empty() {
            let env = profile_env
                .iter()
                .map(|(name, value)| format!("{name} = {value:?}\n"))
                .collect::<Vec<_>>()
                .concat();
            let body = format!(
                "base = \"cline\"\npackage = \"pohunek.runtime.cline\"\ndigest = \"{}\"\nprogram = {:?}\nargs = {:?}\n[env]\n{env}",
                crate::agent::host::fixture::PACKAGE_DIGEST,
                program.display().to_string(),
                args,
            );
            config.agents_dir = Some(temp_agents_dir_with(tag, "clineenv", &body));
            agent = "clineenv";
        }
        let (runtime_root, state_root) = test_worker_roots(&config);
        config.supervision = Some(crate::session::test_supervision(&runtime_root, &state_root));
        config.worker_runtime_root = Some(runtime_root.clone());
        config.worker_state_root = Some(state_root.clone());
        let spawner: Arc<dyn crate::runtime::WorkerLauncher> = Arc::new(
            crate::runtime::InProcessWorkerLauncher::new(runtime_root, state_root.clone()),
        );
        let registry = SessionRegistry::new_with_runtimes_and_launcher(
            config,
            host,
            spawner,
            Arc::new(ReadableHost::new()),
        );
        let session = registry
            .create(SessionNewParams {
                agent: agent.to_owned(),
                cwd: Some(dir.clone()),
                ..params()
            })
            .await?;
        wait_for_file_contains(&launches, "sandbox=").await;
        Ok(Self {
            registry,
            session,
            commands,
            acks,
            hook_runs,
            launches,
            state_root,
            cline_home,
        })
    }

    /// Runs the reporter file of `event` with the fixture `payload` as Cline
    /// would and waits until the fake Cline finished it.
    async fn fire(&self, event: &str, payload: &str) {
        self.fire_as(event, payload, LOCAL).await;
    }

    /// [`Self::fire`] in the environment `mode` names.
    async fn fire_as(&self, event: &str, payload: &str, mode: &str) {
        let before = fs::read_to_string(&self.acks)
            .unwrap_or_default()
            .lines()
            .count();
        let mut writer = &self.commands;
        writer
            .write_all(
                format!("fire {event} {} {mode}\n", fixture_path(payload).display()).as_bytes(),
            )
            .expect("signal the fake Cline");
        wait_until("the hook to finish", || async {
            let now = fs::read_to_string(&self.acks)
                .unwrap_or_default()
                .lines()
                .count();
            (now > before).then_some(())
        })
        .await;
    }

    /// Asserts that exactly `count` hook files ran and every one exited 0
    /// without writing to stdout or stderr.
    fn assert_hooks_quiet(&self, count: usize) {
        let runs: Vec<String> = fs::read_to_string(&self.hook_runs)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            runs,
            vec!["rc=0 bytes=0".to_owned(); count],
            "every reporter run exits 0 and prints nothing"
        );
    }

    async fn worker(&self) -> Worker {
        live_worker_and_identity(&self.registry, &self.session.id)
            .await
            .0
    }

    /// Waits until the worker's active identity names `reference`.
    async fn wait_identity(&self, reference: &str) -> pohunek_worker_protocol::InspectSnapshot {
        let worker = self.worker().await;
        wait_until("the worker to hold the reported identity", || async {
            let snapshot = worker.inspect().await.ok()?;
            let named = snapshot
                .active_identity
                .as_ref()
                .and_then(|identity| identity.native_reference.as_deref());
            (named == Some(reference)).then_some(snapshot)
        })
        .await
    }

    /// Waits until the subagent `id` is in `phase`.
    async fn wait_subagent(&self, id: &str, phase: pohunek_worker_protocol::SubagentPhase) {
        let worker = self.worker().await;
        wait_until("the subagent to reach its phase", || async {
            let snapshot = worker.inspect().await.ok()?;
            snapshot
                .subagents
                .iter()
                .any(|subagent| subagent.id == id && subagent.phase == phase)
                .then_some(())
        })
        .await;
    }

    /// The hook trace lines that name something other than a reply that did not
    /// arrive within the reporter's socket budget.
    ///
    /// A hook that waits for the worker's answer for longer than that budget
    /// traces `worker_timeout` although the worker has applied the report; a
    /// caller that waited for the report's effect (`wait_identity`) knows the
    /// report was delivered, so the timeout says nothing about it.
    fn trace_without_reply_timeouts(&self) -> String {
        self.trace()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .is_none_or(|trace| trace["reason"] != "worker_timeout")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn trace(&self) -> String {
        let trace = self
            .state_root
            .join(&self.session.id.0)
            .join(pohunek_logging::config::HOOK_TRACE_FILE_NAME);
        fs::read_to_string(trace).unwrap_or_default()
    }

    async fn finish(&self) {
        self.registry
            .stop(&self.session.id)
            .await
            .expect("stop the fixture session");
        self.registry.settle_transactions().await;
    }

    async fn remove(&self) {
        self.registry
            .remove(&self.session.id)
            .await
            .expect("remove the fixture session");
    }
}

/// Names and sizes of the entries of `dir`, in name order.
fn entries(dir: &Path) -> Vec<(String, u64)> {
    let mut found: Vec<(String, u64)> = fs::read_dir(dir)
        .expect("read the directory")
        .map(|entry| {
            let entry = entry.expect("directory entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.metadata().expect("entry metadata").len(),
            )
        })
        .collect();
    found.sort();
    found
}

/// Absolute path of the payload fixture `name`, or `name` itself when it is
/// already a path.
fn fixture_path(name: &str) -> PathBuf {
    if Path::new(name).is_absolute() {
        return PathBuf::from(name);
    }
    pohunek_test_support::manifest_dir()
        .join("src/integration/cline/fixtures")
        .join(format!("{name}.json"))
}

/// The fake `cline-bin-<release>`: logs its argv and the sandbox variables it
/// was launched with, and on `fire` runs the installed hook file of an event
/// with the payload on stdin as a direct child, so the hook's `$PPID` is the
/// fake Cline. Each hook's exit status and combined output size is appended to
/// `hook_runs`.
fn fake_cline(
    fifo: &Path,
    acks: &Path,
    hook_runs: &Path,
    launches: &Path,
    hooks: &Path,
    wrapper: &Path,
) -> String {
    format!(
        r#"#!/bin/bash
printf '%s\n' "$*" >> '{launches}'
printf 'sandbox=%s data=%s\n' "${{CLINE_SANDBOX:-}}" "${{CLINE_SANDBOX_DATA_DIR:-}}" >> '{launches}'
while read -r command event payload mode < '{fifo}'; do
  case "$command" in
    fire)
      hook='{hooks}'/"$event".sh
      finish() {{
        printf 'rc=%s bytes=%s\n' "$1" "$(wc -c < '{hook_runs}.out' | tr -d ' ')" >> '{hook_runs}'
        printf 'fired %s\n' "$event" >> '{acks}'
      }}
      case "$mode" in
        hub) env -u CLINE_SANDBOX -u CLINE_SANDBOX_DATA_DIR "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
        foreign) env -i PATH="$PATH" HOME="$HOME" "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
        nosocket) env POHUNEK_WORKER_SOCKET_PATH=/nonexistent/pohunek.sock "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
        unsetsocket) env -u POHUNEK_WORKER_SOCKET_PATH "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
        noinstance) env -u POHUNEK_WORKER_INSTANCE_ID "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
        nokind) env -u POHUNEK_NATIVE_REFERENCE_KIND "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
        detached)
          # The launcher shell backgrounds the wrapper with its own pid (`$$`, a
          # POSIX parameter) and exits; the wrapper runs the hook once that pid
          # is gone, so the hook's parent is a process outside the PTY's tree.
          sh -c '"$0" "$@" "$$" &' '{wrapper}' "$hook" "$payload" '{hook_runs}' "$event" '{acks}' ;;
        *) "$hook" < "$payload" > '{hook_runs}.out' 2>&1; finish $? ;;
      esac ;;
    exit) exit 0 ;;
  esac
done
"#,
        fifo = fifo.display(),
        acks = acks.display(),
        hook_runs = hook_runs.display(),
        launches = launches.display(),
        hooks = hooks.display(),
        wrapper = wrapper.display(),
    )
}

/// The script that runs a hook after its launcher is gone: `hook payload
/// hook_runs event acks launcher_pid`.
const DETACHED_WRAPPER: &str = r#"#!/bin/sh
hook=$1; payload=$2; runs=$3; event=$4; acks=$5; launcher=$6
while kill -0 "$launcher" 2>/dev/null; do :; done
"$hook" < "$payload" > "$runs.out" 2>&1
st=$?
printf 'rc=%s bytes=%s\n' "$st" "$(wc -c < "$runs.out" | tr -d ' ')" >> "$runs"
printf 'fired %s\n' "$event" >> "$acks"
"#;

#[tokio::test]
async fn a_launch_runs_in_sandbox_mode_on_the_users_data_root_without_auto_approve() {
    let rig = Rig::new("cline-launch")
        .await
        .expect("create the Cline session");

    let log = fs::read_to_string(&rig.launches).expect("launch log");
    let data = rig.cline_home.join("data");
    assert!(
        log.contains(&format!("sandbox=1 data={}", data.display())),
        "the agent saw the sandbox pair: {log}"
    );
    let argv = log.lines().next().expect("argv line");
    assert!(
        argv.contains("--id "),
        "the assigned reference is passed: {argv}"
    );
    assert!(
        !argv.contains("--auto-approve"),
        "Cline's own approval setting applies: {argv}"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_profile_data_root_or_config_dir_decides_the_sandbox_data_root() {
    let data = temp_dir("cline-data-root");
    let rig = Rig::start(
        "cline-profile-data",
        &[],
        &[("CLINE_DATA_DIR", data.to_str().expect("utf-8 path"))],
    )
    .await
    .expect("create the Cline session");
    let log = fs::read_to_string(&rig.launches).expect("launch log");
    assert!(
        log.contains(&format!("sandbox=1 data={}\n", data.display())),
        "{log}"
    );
    rig.finish().await;
    rig.remove().await;

    let config = temp_dir("cline-config-dir");
    crate::integration::install_cline(&config).expect("install into the profile config dir");
    let rig = Rig::start(
        "cline-profile-dir",
        &[],
        &[("CLINE_DIR", config.to_str().expect("utf-8 path"))],
    )
    .await
    .expect("create the Cline session");
    let log = fs::read_to_string(&rig.launches).expect("launch log");
    assert!(
        log.contains(&format!(
            "sandbox=1 data={}\n",
            config.join("data").display()
        )),
        "{log}"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_launch_that_overrides_sandbox_mode_is_refused_with_a_typed_error() {
    let data = temp_dir("cline-refused");
    let data = data.to_str().expect("utf-8 path");
    for flag in ["--data-dir", "--data-dir=/x", "--config", "--config=/x"] {
        let error = Rig::start("cline-refused-arg", &[flag], &[])
            .await
            .err()
            .expect("the flag refuses the launch");
        assert_eq!(error.code, "cline_launch_arg_refused", "{flag}");
    }
    for name in [
        "CLINE_SANDBOX",
        "CLINE_SANDBOX_DATA_DIR",
        "CLINE_SESSION_DATA_DIR",
        "CLINE_DB_DATA_DIR",
        "CLINE_PROVIDER_SETTINGS_PATH",
    ] {
        let error = Rig::start("cline-refused-env", &[], &[(name, data)])
            .await
            .err()
            .expect("the variable refuses the launch");
        assert_eq!(error.code, "cline_launch_env_refused", "{name}");
        assert!(
            error.msg.contains(name) && !error.msg.contains(data),
            "{}",
            error.msg
        );
    }
    for value in ["relative/data", ""] {
        let error = Rig::start("cline-refused-data-root", &[], &[("CLINE_DATA_DIR", value)])
            .await
            .err()
            .expect("a relative or empty data root refuses the launch");
        assert_eq!(error.code, "cline_data_root_invalid", "{value:?}");
    }
    let error = Rig::start("cline-refused-empty-sandbox", &[], &[("CLINE_SANDBOX", "")])
        .await
        .err()
        .expect("an empty sandbox value refuses the launch");
    assert_eq!(error.code, "cline_launch_env_refused");
    let error = Rig::start_with("cline-no-home", &[], &[], false)
        .await
        .err()
        .expect("a descriptor without a config home refuses the launch");
    assert_eq!(error.code, "cline_config_home_missing");
}

#[tokio::test]
async fn a_root_turn_binds_and_clear_and_fork_rebind_at_the_next_turn() {
    let rig = Rig::new("cline-bind")
        .await
        .expect("create the Cline session");

    rig.fire("TaskResume", "root_resume").await;
    let snapshot = rig.wait_identity(ROOT).await;
    assert!(
        snapshot.launch_identity.is_some(),
        "the cline-bin process is the launch process"
    );

    rig.fire("TaskResume", "root_resume_after_clear").await;
    rig.wait_identity(AFTER_CLEAR).await;
    rig.fire("TaskResume", "root_resume_after_fork").await;
    rig.wait_identity(AFTER_FORK).await;
    rig.fire("TaskStart", "root_start").await;
    rig.wait_identity(GENERATED).await;

    rig.assert_hooks_quiet(4);
    let trace = rig.trace_without_reply_timeouts();
    assert!(trace.is_empty(), "accepted reports leave no trace: {trace}");
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn the_end_of_the_lead_turn_does_not_release_or_change_the_identity() {
    let rig = Rig::new("cline-lead-end")
        .await
        .expect("create the Cline session");
    rig.fire("TaskResume", "root_resume").await;
    rig.wait_identity(ROOT).await;

    rig.fire("TaskComplete", "root_complete").await;
    rig.fire("TaskError", "root_error").await;
    rig.fire("SessionShutdown", "root_shutdown").await;

    let snapshot = rig.worker().await.inspect().await.expect("inspect");
    assert_eq!(
        snapshot
            .active_identity
            .as_ref()
            .and_then(|identity| identity.native_reference.as_deref()),
        Some(ROOT)
    );
    assert!(snapshot.subagents.is_empty());
    rig.assert_hooks_quiet(4);
    let trace = rig.trace_without_reply_timeouts();
    assert!(trace.is_empty(), "{trace}");
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn subagent_turns_journal_and_end_with_their_outcome() {
    use pohunek_worker_protocol::SubagentPhase;
    let rig = Rig::new("cline-subagent")
        .await
        .expect("create the Cline session");
    rig.fire("TaskResume", "root_resume").await;
    rig.wait_identity(ROOT).await;

    rig.fire("TaskResume", "subagent_resume").await;
    rig.wait_subagent(SUBAGENT, SubagentPhase::Running).await;
    let worker = rig.worker().await;
    let running = worker.inspect().await.expect("inspect");
    let subagent = &running.subagents[0];
    assert_eq!(
        subagent.parent_id.as_deref(),
        Some("agent_00000000-0000-4000-8000-000000000001")
    );
    assert_eq!(subagent.agent_type, None, "Cline names no role");

    rig.fire("TaskComplete", "subagent_complete").await;
    rig.wait_subagent(SUBAGENT, SubagentPhase::Completed).await;

    rig.fire("TaskResume", "subagent_resume").await;
    rig.wait_subagent(SUBAGENT, SubagentPhase::Running).await;
    rig.fire("TaskError", "subagent_error").await;
    rig.wait_subagent(SUBAGENT, SubagentPhase::Failed).await;

    rig.fire("TaskResume", "subagent_resume_escape").await;
    rig.wait_subagent(ESCAPED_SUBAGENT, SubagentPhase::Running)
        .await;
    rig.fire("SessionShutdown", "subagent_shutdown").await;
    rig.wait_subagent(ESCAPED_SUBAGENT, SubagentPhase::Cancelled)
        .await;

    rig.fire("TaskResume", "subagent_resume_escape").await;
    rig.wait_subagent(ESCAPED_SUBAGENT, SubagentPhase::Running)
        .await;
    rig.fire("TaskCancel", "subagent_abort").await;
    rig.wait_subagent(ESCAPED_SUBAGENT, SubagentPhase::Cancelled)
        .await;

    rig.assert_hooks_quiet(9);
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_subagent_stop_without_a_recorded_start_is_kept_as_terminal() {
    use pohunek_worker_protocol::SubagentPhase;
    let rig = Rig::new("cline-stop-first")
        .await
        .expect("create the Cline session");
    rig.fire("TaskResume", "root_resume").await;
    rig.wait_identity(ROOT).await;

    rig.fire("TaskComplete", "subagent_complete").await;

    rig.wait_subagent(SUBAGENT, SubagentPhase::Completed).await;
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_hook_of_a_hub_hosted_session_reports_nothing() {
    let rig = Rig::new("cline-hub")
        .await
        .expect("create the Cline session");

    rig.fire_as("TaskStart", "hub_start", HUB).await;
    rig.fire_as("TaskResume", "hub_resume", HUB).await;
    rig.fire_as("TaskResume", "subagent_resume", HUB).await;

    rig.assert_hooks_quiet(3);
    let snapshot = rig.worker().await.inspect().await.expect("inspect");
    assert!(
        snapshot.active_identity.is_none(),
        "no hub hook binds an identity"
    );
    assert!(snapshot.subagents.is_empty());
    assert_eq!(
        rig.trace().matches("hub_hosted").count(),
        3,
        "{}",
        rig.trace()
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_cline_turn_outside_pohunek_exits_without_output_or_effect() {
    let rig = Rig::new("cline-foreign")
        .await
        .expect("create the Cline session");
    let state = rig.state_root.join(&rig.session.id.0);
    let before = entries(&state);

    rig.fire_as("TaskResume", "root_resume", FOREIGN).await;
    rig.fire_as("SessionShutdown", "subagent_shutdown", FOREIGN)
        .await;

    rig.assert_hooks_quiet(2);
    assert_eq!(entries(&state), before, "the session state is untouched");
    assert!(rig.trace().is_empty());
    let snapshot = rig.worker().await.inspect().await.expect("inspect");
    assert!(snapshot.active_identity.is_none());
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn malformed_or_unsupported_input_exits_quietly_and_names_its_reason_in_the_trace() {
    let rig = Rig::new("cline-malformed")
        .await
        .expect("create the Cline session");
    let oversized = temp_dir("cline-malformed-big");
    fs::write(
        oversized.join("big.json"),
        format!(r#"{{"pad":"{}"}}"#, "x".repeat(70 * 1024)),
    )
    .expect("write the oversized payload");

    rig.fire("TaskResume", "malformed_not_json").await;
    rig.fire("TaskResume", "malformed_empty").await;
    rig.fire("TaskResume", "root_resume_no_session").await;
    rig.fire("TaskResume", "root_resume_bad_session").await;
    rig.fire("TaskResume", "root_resume_colon_session").await;
    rig.fire("TaskResume", "malformed_array").await;
    rig.fire("TaskResume", "tool_call").await;
    rig.fire(
        "TaskResume",
        oversized.join("big.json").to_str().expect("utf-8 path"),
    )
    .await;

    rig.assert_hooks_quiet(8);
    let trace = rig.trace();
    for (detail, lines) in [
        ("not_json", 1),
        ("empty", 1),
        ("not_object", 1),
        ("root_session_id_missing", 1),
        // The bad id and the colon id are each refused by the id check alone.
        ("root_session_id_invalid", 2),
    ] {
        assert_eq!(
            trace.matches(&format!("\"detail\":\"{detail}\"")).count(),
            lines,
            "trace lines naming {detail}: {trace}"
        );
    }
    for reason in ["event_unsupported", "input_too_large"] {
        assert_eq!(trace.matches(reason).count(), 1, "{reason}: {trace}");
    }
    assert!(
        rig.worker()
            .await
            .inspect()
            .await
            .expect("inspect")
            .active_identity
            .is_none(),
        "no malformed input binds an identity"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn every_launch_of_the_conversation_carries_the_sandbox_pair() {
    let rig = Rig::new("cline-resume")
        .await
        .expect("create the Cline session");
    rig.fire("TaskResume", "root_resume").await;
    rig.wait_identity(ROOT).await;
    rig.finish().await;

    rig.registry
        .resume(&rig.session.id)
        .await
        .expect("resume the conversation");
    wait_until("the relaunch to log its argv", || async {
        let log = fs::read_to_string(&rig.launches).unwrap_or_default();
        (log.matches("sandbox=1").count() == 2).then_some(())
    })
    .await;
    let log = fs::read_to_string(&rig.launches).expect("launch log");
    for argv in log.lines().filter(|line| line.contains("--id ")) {
        assert!(!argv.contains("--auto-approve"), "{argv}");
    }
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_hook_outside_the_pty_tree_is_refused_by_the_worker_even_with_the_sandbox_variables() {
    let rig = Rig::new("cline-detached")
        .await
        .expect("create the Cline session");

    rig.fire_as("TaskResume", "root_resume", DETACHED).await;

    rig.assert_hooks_quiet(1);
    assert!(rig.trace().contains("worker_refused"), "{}", rig.trace());
    let snapshot = rig.worker().await.inspect().await.expect("inspect");
    assert!(
        snapshot.active_identity.is_none(),
        "an outside hook binds nothing"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_hook_without_a_usable_worker_route_traces_why_and_reports_nothing() {
    let rig = Rig::new("cline-route")
        .await
        .expect("create the Cline session");

    rig.fire_as("TaskResume", "root_resume", NO_SOCKET).await;
    rig.fire_as("TaskResume", "root_resume", UNSET_SOCKET).await;
    rig.fire_as("TaskResume", "root_resume", NO_INSTANCE).await;

    rig.assert_hooks_quiet(3);
    let trace = rig.trace();
    assert_eq!(trace.matches("worker_socket_missing").count(), 2, "{trace}");
    assert_eq!(
        trace.matches("worker_instance_missing").count(),
        1,
        "{trace}"
    );
    let snapshot = rig.worker().await.inspect().await.expect("inspect");
    assert!(snapshot.active_identity.is_none());
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_reference_kind_other_than_id_reports_the_identity_without_a_reference() {
    let rig = Rig::new("cline-kind")
        .await
        .expect("create the Cline session");

    rig.fire_as("TaskResume", "root_resume", NO_KIND).await;

    let worker = rig.worker().await;
    let snapshot = wait_until("the identity to be reported", || async {
        let snapshot = worker.inspect().await.ok()?;
        snapshot.active_identity.is_some().then_some(snapshot)
    })
    .await;
    assert_eq!(
        snapshot
            .active_identity
            .as_ref()
            .and_then(|identity| identity.native_reference.as_deref()),
        None,
        "no reference is sent without the kind"
    );
    rig.finish().await;
    rig.remove().await;
}
