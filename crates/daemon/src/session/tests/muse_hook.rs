//! The Muse hook handler across a real worker, the launch-input seam and a fake
//! `muse-bin-<release>` that runs the overlay's hooks with a cleared environment.
//!
//! UNVERIFIED: the payload fixtures under `integration/muse/fixtures` and the
//! managed hooks file layout come from the Muse documentation and the MIT SDK
//! schema (release 1.4.13), not from a capture of a real Muse run.

// Rust guideline compliant 2026-10-27

use protocol::ProtocolError;

use super::*;
use crate::agent::host::{
    BuiltinSource, DefinitionError, RuntimeRegistry, RuntimeSource, SourceTrust,
};
use crate::detect::generic_shell_manifest;
use protocol::PackageDigest;

/// Root session id of the fixture payloads.
const ROOT: &str = "aaaaaaaa-0000-4000-8000-000000000001";
/// Root session id after `/clear` in the fixture payloads.
const CLEARED: &str = "bbbbbbbb-0000-4000-8000-000000000002";
/// File name of the fake Muse executable; a release binary is named this way.
const MUSE_BIN: &str = "muse-bin-1.4.13-R0";

/// A test source serving the Muse runtime as an official package.
#[derive(Debug)]
struct MuseSource(RuntimeDefinition);

impl RuntimeSource for MuseSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Official
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(vec![self.0.clone()])
    }
}

fn muse_host(program: &Path, script: &Path, extra_args: &[&str]) -> RuntimeHost {
    let args: Vec<String> = std::iter::once(script.display().to_string())
        .chain(extra_args.iter().map(|arg| (*arg).to_owned()))
        .collect();
    let document = format!(
        r#"schema = 1
id = "pohunek.runtime.muse"
version = "1.0.0"
runtime_api = 1
requires = ["config-home-subdir-v1", "integration-muse-hook-v1"]

[runtime]
id = "muse"
name = "Muse Code"
program = {program:?}
args = {args:?}
detect_manifest = "codex"
prompt_arg = false

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
handler = "muse-hook-v1"
hook_schema = "identity-subagent-v1"

[config_home]
env = "XDG_CONFIG_HOME"
subdir = "muse"
default = ".config/muse"
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
    .expect("fixture Muse descriptor");
    let builtin = BuiltinSource::new("/bin/sh");
    let official = MuseSource(definition);
    RuntimeHost::new(RuntimeRegistry::from_sources(&[&builtin, &official]).expect("runtime host"))
}

/// A launch backed by a real worker and a fake Muse.
struct Rig {
    registry: SessionRegistry,
    session: SessionInfo,
    commands: fs::File,
    acks: PathBuf,
    launches: PathBuf,
    state_root: PathBuf,
    muse_home: PathBuf,
}

impl Rig {
    /// `settings` is written as the Muse `settings.json` before the launch.
    async fn new(
        tag: &str,
        settings: Option<&str>,
        extra_args: &[&str],
    ) -> Result<Self, ProtocolError> {
        let dir = temp_dir(tag);
        let xdg = PathBuf::from(
            crate::test_support::thread_environment_variable("XDG_CONFIG_HOME")
                .expect("the hermetic environment names XDG_CONFIG_HOME"),
        );
        let muse_home = xdg.join("muse");
        fs::create_dir_all(&muse_home).expect("create the Muse config home");
        if let Some(settings) = settings {
            fs::write(muse_home.join("settings.json"), settings).expect("write the settings");
        }
        let commands = hook_gate(&dir.join("commands.fifo"));
        let acks = dir.join("acks.txt");
        let launches = dir.join("launches.txt");
        let script = dir.join("muse-agent.sh");
        write_executable(
            &script,
            &fake_muse(&dir.join("commands.fifo"), &acks, &launches),
        );
        let program = dir.join(MUSE_BIN);
        pohunek_test_support::fs::copy_shell_image(native_supersede::bash_path(), &program)
            .expect("copy the fixture agent image");
        let host = muse_host(&program, &script, extra_args);
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
            crate::runtime::InProcessWorkerLauncher::new(runtime_root, state_root.clone()),
        );
        let registry = SessionRegistry::new_with_runtimes_and_launcher(
            config,
            host,
            launcher,
            Arc::new(ReadableHost::new()),
        );
        let session = registry
            .create(SessionNewParams {
                agent: "muse".to_owned(),
                cwd: Some(dir.clone()),
                ..params()
            })
            .await?;
        wait_for_file_contains(&launches, "overlay=").await;
        Ok(Self {
            registry,
            session,
            commands,
            acks,
            launches,
            state_root,
            muse_home,
        })
    }

    /// Runs the hooks of `event` with `payload` as Muse would, in a cleared
    /// environment, and waits until the fake Muse finished them.
    async fn fire(&self, event: &str, payload: &str, root: &str) {
        let fixture = fixture_path(payload);
        let before = fs::read_to_string(&self.acks)
            .unwrap_or_default()
            .lines()
            .count();
        let mut writer = &self.commands;
        writer
            .write_all(format!("fire {event} {} {root}\n", fixture.display()).as_bytes())
            .expect("signal the fake Muse");
        wait_until("the hooks to finish", || async {
            let now = fs::read_to_string(&self.acks)
                .unwrap_or_default()
                .lines()
                .count();
            (now > before).then_some(())
        })
        .await;
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

    fn overlay_path(&self) -> PathBuf {
        self.state_root
            .join(&self.session.id.0)
            .join(pohunek_paths::launch_input_file_name("muse-hooks.json"))
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

/// Absolute path of the payload fixture `name`.
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/integration/muse/fixtures")
        .join(format!("{name}.json"))
}

/// The fake `muse-bin-<release>`: logs its launch, reads the `-c
/// managed_hooks_env_vars` list and the overlay named by
/// `TBH_MANAGED_HOOKS_PATH`, and on `fire` runs the overlay's hooks for an
/// event in a cleared environment that keeps only the listed variables plus
/// `MUSE_SESSION_ID` and `MUSE_THREAD_ID`, with the payload on stdin.
fn fake_muse(fifo: &Path, acks: &Path, launches: &Path) -> String {
    format!(
        r#"#!/bin/bash
printf '%s\n' "$*" >> '{launches}'
printf 'overlay=%s\n' "${{TBH_MANAGED_HOOKS_PATH:-}}" >> '{launches}'
names=
prev=
for arg in "$@"; do
  if [ "$prev" = "-c" ]; then
    case "$arg" in
      managed_hooks_env_vars=*)
        names=$(python3 -I -c 'import json,sys; print(" ".join(json.loads(sys.argv[1].split("=", 1)[1])))' "$arg") ;;
    esac
  fi
  prev=$arg
done
hook_commands() {{
  python3 -I -c 'import json,sys
overlay = json.load(open(sys.argv[1]))
for entry in overlay["hooks"].get(sys.argv[2], []):
    for hook in entry["hooks"]:
        print(hook["command"])' "$TBH_MANAGED_HOOKS_PATH" "$1"
}}
while read -r command event payload root < '{fifo}'; do
  case "$command" in
    fire)
      assign=
      for name in $names; do
        value=$(printenv "$name") || continue
        assign="$assign $name=$(printf '%q' "$value")"
      done
      while IFS= read -r hook; do
        eval "env -i$assign MUSE_SESSION_ID=$root MUSE_THREAD_ID=$root $hook" < "$payload"
      done < <(hook_commands "$event")
      printf 'fired %s\n' "$event" >> '{acks}' ;;
    exit) exit 0 ;;
  esac
done
"#,
        fifo = fifo.display(),
        acks = acks.display(),
        launches = launches.display(),
    )
}

#[tokio::test]
async fn a_root_session_start_binds_and_reminder_or_child_threads_are_refused() {
    let rig = Rig::new("muse-bind", None, &[])
        .await
        .expect("create the Muse session");

    rig.fire("SessionStart", "session_start_reminder", ROOT)
        .await;
    rig.fire("SessionStart", "session_start_child", ROOT).await;
    rig.fire("SessionStart", "session_start_unknown_source", ROOT)
        .await;
    rig.fire("SessionStart", "session_start_startup", ROOT)
        .await;

    let snapshot = rig.wait_identity(ROOT).await;
    assert!(
        snapshot.launch_identity.is_some(),
        "the muse-bin process is the launch process"
    );
    let trace = rig.trace();
    for reason in ["reminder_thread", "thread_not_root", "source_unknown"] {
        assert!(
            trace.contains(reason),
            "a trace line names {reason}: {trace}"
        );
    }
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn clear_rebinds_the_session_to_the_new_conversation() {
    let rig = Rig::new("muse-clear", None, &[])
        .await
        .expect("create the Muse session");
    rig.fire("SessionStart", "session_start_startup", ROOT)
        .await;
    rig.wait_identity(ROOT).await;

    rig.fire("SessionStart", "session_start_clear", CLEARED)
        .await;

    rig.wait_identity(CLEARED).await;
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn subagent_start_and_stop_journal_and_complete_without_a_role_or_reminders() {
    let rig = Rig::new("muse-subagent", None, &[])
        .await
        .expect("create the Muse session");
    rig.fire("SessionStart", "session_start_startup", ROOT)
        .await;
    rig.wait_identity(ROOT).await;

    rig.fire("SubagentStart", "subagent_start_reminder", ROOT)
        .await;
    rig.fire("SubagentStart", "subagent_start", ROOT).await;
    let worker = rig.worker().await;
    let running = wait_until("the subagent start to journal", || async {
        let snapshot = worker.inspect().await.ok()?;
        (!snapshot.subagents.is_empty()).then_some(snapshot)
    })
    .await;
    assert_eq!(running.subagents.len(), 1, "the reminder child is dropped");
    assert_eq!(running.subagents[0].id, "sub-1");
    assert_eq!(running.subagents[0].agent_type, None, "Muse names no role");
    assert_eq!(running.subagents[0].parent_id.as_deref(), Some(ROOT));

    rig.fire("SubagentStop", "subagent_stop", ROOT).await;
    wait_until("the subagent to complete", || async {
        let snapshot = worker.inspect().await.ok()?;
        snapshot
            .subagents
            .iter()
            .all(|subagent| subagent.finished_at_ms.is_some())
            .then_some(())
    })
    .await;
    assert!(rig.trace().contains("reminder_thread"));
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn create_and_every_resume_carry_the_overlay_env_and_the_variable_list_once() {
    let rig = Rig::new("muse-resume", None, &[])
        .await
        .expect("create the Muse session");
    rig.fire("SessionStart", "session_start_startup", ROOT)
        .await;
    let snapshot = rig.wait_identity(ROOT).await;
    assert!(snapshot.native_reference.is_some());
    let worker = rig.worker().await;
    let settled = worker.inspect().await.expect("inspect");
    let outcome = wait_until("daemon to settle the Muse identity", || async {
        match Box::pin(
            rig.registry
                .apply_worker_metadata_snapshot(&rig.session.id, &settled),
        )
        .await
        {
            WorkerMetadataApplyOutcome::Retryable(_) => None,
            settled => Some(settled),
        }
    })
    .await;
    assert_eq!(outcome, WorkerMetadataApplyOutcome::Applied);
    rig.finish().await;

    for round in 1..=2 {
        rig.registry
            .resume(&rig.session.id)
            .await
            .expect("resume the conversation");
        wait_until("the relaunch to log its argv", || async {
            let log = fs::read_to_string(&rig.launches).unwrap_or_default();
            (log.matches(&format!("resume {ROOT}")).count() == round).then_some(())
        })
        .await;
        rig.finish().await;
    }

    let log = fs::read_to_string(&rig.launches).expect("launch log");
    let launches: Vec<&str> = log.lines().filter(|line| line.starts_with("-c ")).collect();
    assert_eq!(launches.len(), 3, "create and two resumes: {log}");
    for line in &launches {
        assert_eq!(line.matches("managed_hooks_env_vars").count(), 1, "{line}");
    }
    assert!(launches[1].ends_with(&format!("resume {ROOT}")));
    assert_eq!(
        log.matches(&format!("overlay={}", rig.overlay_path().display()))
            .count(),
        3,
        "every launch selects the overlay: {log}"
    );
    rig.remove().await;
}

#[tokio::test]
async fn the_overlay_and_reporter_live_in_the_state_directory_with_private_modes() {
    let rig = Rig::new("muse-placement", None, &[])
        .await
        .expect("create the Muse session");
    let overlay = rig.overlay_path();
    let reporter =
        overlay.with_file_name(pohunek_paths::launch_input_file_name("muse-reporter.sh"));

    assert_eq!(
        fs::metadata(&overlay)
            .expect("overlay")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&reporter)
            .expect("reporter")
            .permissions()
            .mode()
            & 0o777,
        0o500
    );
    let text = fs::read_to_string(&overlay).expect("overlay text");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("overlay JSON");
    let command = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("command");
    assert_eq!(command, format!("'{}' session", reporter.display()));
    assert!(
        overlay.starts_with(&rig.state_root),
        "outside TMPDIR, worktree and config home"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_foreign_managed_hooks_file_is_kept_verbatim_and_ours_is_appended() {
    let dir = temp_dir("muse-foreign-file");
    let foreign = dir.join("policy-hooks.json");
    fs::write(
        &foreign,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"policy-check"}]}],"Stop":[{"hooks":[{"type":"command","command":"audit"}]}]}}"#,
    )
    .expect("foreign file");
    let settings = format!(
        r#"{{"schema_version":1,"managed_hooks_path":{:?},"managed_hooks_env_vars":["MY_AUDIT_TOKEN_DIR"]}}"#,
        foreign.display().to_string()
    );
    let rig = Rig::new("muse-foreign", Some(&settings), &[])
        .await
        .expect("create the Muse session");

    let parsed: serde_json::Value =
        serde_json::from_slice(&fs::read(rig.overlay_path()).expect("overlay")).expect("JSON");
    let start = parsed["hooks"]["SessionStart"].as_array().expect("events");
    assert_eq!(start.len(), 2);
    assert_eq!(start[0]["hooks"][0]["command"], "policy-check");
    assert_eq!(parsed["hooks"]["Stop"][0]["hooks"][0]["command"], "audit");
    assert!(parsed["hooks"]["SubagentStop"].is_array());
    let log = fs::read_to_string(&rig.launches).expect("launch log");
    assert!(
        log.contains("MY_AUDIT_TOKEN_DIR"),
        "the user's list is unioned: {log}"
    );
    assert!(log.contains("POHUNEK_SESSION_ID"), "{log}");
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn an_unreadable_or_unparsable_foreign_file_refuses_the_launch() {
    for (tag, body) in [
        ("muse-foreign-garbage", Some("not json")),
        ("muse-foreign-missing", None),
    ] {
        let dir = temp_dir(tag);
        let foreign = dir.join("policy-hooks.json");
        if let Some(body) = body {
            fs::write(&foreign, body).expect("foreign file");
        }
        let settings = format!(
            r#"{{"schema_version":1,"managed_hooks_path":{:?}}}"#,
            foreign.display().to_string()
        );
        let error = Rig::new(tag, Some(&settings), &[])
            .await
            .err()
            .expect("refused");
        assert_eq!(error.code, "integration_overlay_unavailable", "{tag}");
    }
}

#[tokio::test]
async fn a_launch_argument_that_sets_the_variable_list_is_refused() {
    let error = Rig::new("muse-dup", None, &["-c", "managed_hooks_env_vars=[\"X\"]"])
        .await
        .err()
        .expect("refused");
    assert_eq!(error.code, "integration_launch_arg_conflict");
}

#[tokio::test]
async fn install_status_and_uninstall_write_nothing_into_the_muse_home() {
    let settings = r#"{"schema_version":1,"mcp_servers":{}}"#;
    let rig = Rig::new("muse-lifecycle", Some(settings), &[])
        .await
        .expect("create the Muse session");
    let settings_path = rig.muse_home.join("settings.json");
    let before = fs::read(&settings_path).expect("settings");
    let names_before = fs::read_dir(&rig.muse_home).expect("home").count();
    let homes = crate::integration::ConfigHomes::new(
        rig.registry.profiles().clone(),
        rig.registry.inspect_base_environment(),
    );
    let agent = protocol::RuntimeRef::from_wire("muse");

    let installed = crate::integration::install_in(
        &homes,
        Some(&agent),
        &crate::integration::HomeSelection::Runtime,
        &crate::integration::RetainedSchemas::default(),
    )
    .expect("install");
    assert_eq!(installed.installed.len(), 1);
    assert!(installed.installed[0].config_paths.is_empty());
    let status = crate::integration::status_in(
        &homes,
        protocol::IntegrationStatusParams {
            agent: Some(agent.clone()),
            profile: None,
            all_profiles: false,
        },
    )
    .expect("status");
    assert_eq!(status.agents.len(), 1);
    assert_eq!(
        status.agents[0].state,
        protocol::IntegrationInstallState::Current
    );
    let removed = crate::integration::uninstall_in(
        &homes,
        &agent,
        &crate::integration::HomeSelection::Runtime,
    )
    .expect("uninstall");
    assert_eq!(removed.uninstalled.len(), 1);
    assert!(removed.uninstalled[0].removed_paths.is_empty());

    assert_eq!(fs::read(&settings_path).expect("settings"), before);
    assert_eq!(
        fs::read_dir(&rig.muse_home).expect("home").count(),
        names_before
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_malformed_settings_file_is_reported_by_status_and_refuses_the_launch() {
    let error = Rig::new("muse-bad-settings", Some("{broken"), &[])
        .await
        .err()
        .expect("refused");
    assert_eq!(error.code, "integration_overlay_unavailable");
}

#[tokio::test]
async fn reporter_asset_reads_exactly_the_listed_pohunek_variables() {
    let asset = include_str!("../../integration/muse/reporter.sh");
    // Names the script sets for itself rather than reads from the hook
    // environment.
    let own = ["POHUNEK_AGENT_PID", "POHUNEK_HOOK_ACTION"];
    let mut read: Vec<&str> = asset
        .split(|ch: char| !(ch.is_ascii_uppercase() || ch == '_' || ch.is_ascii_digit()))
        .filter(|word| word.starts_with("POHUNEK_") && word.len() > "POHUNEK_".len())
        .filter(|word| !own.contains(word) && !word.starts_with("POHUNEK_INTEGRATION"))
        .collect();
    read.sort_unstable();
    read.dedup();
    let mut listed: Vec<&str> = crate::integration::muse_hook_env_vars().to_vec();
    listed.sort_unstable();
    assert_eq!(read, listed);
}

#[tokio::test]
async fn an_oversized_or_misshapen_foreign_file_refuses_the_launch() {
    let oversized = format!(r#"{{"pad":"{}"}}"#, "x".repeat(70 * 1024));
    for (tag, body) in [
        ("muse-foreign-big", oversized.as_str()),
        ("muse-foreign-hooks-array", r#"{"hooks":[]}"#),
        (
            "muse-foreign-event-null",
            r#"{"hooks":{"SessionStart":null}}"#,
        ),
    ] {
        let dir = temp_dir(tag);
        let foreign = dir.join("policy-hooks.json");
        fs::write(&foreign, body).expect("foreign file");
        let settings = format!(
            r#"{{"schema_version":1,"managed_hooks_path":{:?}}}"#,
            foreign.display().to_string()
        );
        let error = Rig::new(tag, Some(&settings), &[])
            .await
            .err()
            .expect("refused");
        assert_eq!(error.code, "integration_overlay_unavailable", "{tag}");
        assert!(
            error.msg.contains("policy-hooks.json"),
            "the message names the file: {}",
            error.msg
        );
    }
}
