//! OSC 7501 program status through a real session: worker, PTY and registry.
//!
//! The sessions here run a host profile over the official Claude package whose
//! override manifest is the shipped Claude manifest with its `[program_status]`
//! table set per scenario. The program is a
//! POSIX shell stand-in for a probe-waiting agent: it prints the 7501 probe,
//! waits for the worker's reply, draws a Claude prompt box and reports its
//! state by raw records when it reads a command byte.

// Rust guideline compliant 2026-06-26

#![cfg(target_os = "linux")]

#[path = "support/official_runtimes.rs"]
mod official_runtimes;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{worker_binary, workspace_root};
use protocol::{
    AgentActivity, SessionId, SessionInfo, SessionInputParams, SessionInputWait, SessionNewParams,
    StateSource, ENV_DAEMON_ID, ENV_SESSION_ID,
};

/// `session.detection` as a client that predates the program-status region
/// kind decodes it: a strict closed set of kinds.
#[derive(serde::Deserialize)]
struct LegacyDetection {
    #[serde(rename = "session_id")]
    _session_id: String,
    supported_regions: Vec<LegacyKind>,
    previews: Vec<LegacyPreview>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPreview {
    #[serde(rename = "kind")]
    _kind: LegacyKind,
    #[serde(rename = "region")]
    _region: String,
    #[serde(rename = "text")]
    _text: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyKind {
    OscTitle,
    OscProgress,
    WholeRecent,
    BottomLines,
    BottomNonEmptyLines,
    TopNonEmptyLines,
    LastNonEmptyAbovePromptBox,
    AfterLastPromptMarker,
    PromptBoxBody,
    AfterLastHorizontalRule,
}

const TEST_COLS: u16 = 100;
const TEST_ROWS: u16 = 24;
/// The `app` the stand-in agent's records carry and its manifest opts in with.
const APP: &str = "fake";
/// The reply the worker writes for an answered probe, as hex.
const REPLY_HEX: &str = "1b5d373530313b3f1b5c";
/// Byte length of the reply.
const REPLY_BYTES: usize = 10;
/// Interval of the process scan that recognises a program started below a
/// shell.
const PROCWATCH_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// Bound of a wait that must be satisfied by the stand-in agent.
const WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;

/// The stand-in agent. `$1` is the mode, `$2` the scratch directory and `$3`
/// where commands come from.
///
/// It answers the version probe, switches the terminal to raw mode, prints the
/// probe and, in the `reply` mode, records the first ten bytes it reads (the
/// worker's reply) before it draws a Claude prompt box. A command read from the
/// terminal (`stdin` source) runs on Enter, as a submitted prompt does; one
/// read from the `cmd` FIFO (`fifo` source) runs at once. The commands: `g`
/// reports working, `d` done, `b` blocked and `q` exits; `t` is a turn that
/// reports working until the FIFO yields a byte, then done. `c` and `x` report
/// the child records a Claude background agent emits (a `working` record with
/// an `id` and a `title`, then a bare `clear`); `m` draws a marker line.
fn agent_script() -> String {
    format!(
        r#"#!/bin/sh
{version}
mode=$1; dir=$2; source=$3
stty raw -echo
printf '\033]7501;?\033\\'
if [ "$mode" = reply ]; then
  head -c 10 | od -An -v -tx1 | tr -d ' \n' > "$dir/reply.hex"
fi
emit() {{ printf '\033]7501;state=%s:app={APP}\033\\' "$1"; }}
box() {{
  printf '────────────────────────────────────────\r\n'
  printf '❯ \r\n'
  printf '────────────────────────────────────────\r\n'
  printf '  ? for shortcuts'
}}
printf '\033[2J\033[H'
box
if [ "$mode" = reply ]; then emit idle; fi
if [ "$mode" = silent ] || [ "$mode" = recording ]; then
  head -c 2 | od -An -v -tx1 | tr -d ' \n' > "$dir/typed.hex"
  if [ "$mode" = recording ]; then
    emit working
    printf 'SYNC\r\n'
  fi
  exec sleep 600
fi
exec 3<> "$dir/cmd"
cr=$(printf '\r')
pending=
run() {{
  case "$1" in
    g) emit working ;;
    d) emit done ;;
    b) emit 'blocked:kind=permission' ;;
    t) emit working; head -c 1 <&3 > /dev/null; emit done ;;
    c) printf '\033]7501;state=working:app={APP}:id=a460128b96cbca53d:title=U3R1YiBiYWNrZ3JvdW5kIGFnZW50\033\\' ;;
    x) printf '\033]7501;state=clear:id=a460128b96cbca53d\033\\' ;;
    m) printf 'SYNC\r\n' ;;
    q) exit 0 ;;
  esac
}}
while :; do
  if [ "$source" = fifo ]; then
    run "$(head -c 1 <&3)"
  else
    c=$(head -c 1)
    if [ "$c" = "$cr" ]; then run "$pending"; pending=; else pending=$c; fi
  fi
done
"#,
        version = official_runtimes::sh_version_answer("claude")
    )
}

/// The shipped Claude manifest with its `[program_status]` table replaced by one
/// that sets `answer_probe` and, for the gate scenarios, an `authority_from` the
/// shipped manifest does not declare.
fn opted_in_manifest(answer_probe: bool, authority_from: Option<&str>) -> String {
    let mut in_table = false;
    let shipped =
        std::fs::read_to_string(workspace_root().join("runtime-packages/claude/detect.toml"))
            .expect("read the shipped Claude manifest")
            .lines()
            .filter(|line| {
                if line.trim() == "[program_status]" {
                    in_table = true;
                } else if in_table && line.trim().is_empty() {
                    in_table = false;
                }
                !in_table
            })
            .fold(String::new(), |mut source, line| {
                source.push_str(line);
                source.push('\n');
                source
            });
    let gate = authority_from
        .map(|version| format!("authority_from = \"{version}\"\n"))
        .unwrap_or_default();
    format!("{shipped}\n[program_status]\napp = \"{APP}\"\nanswer_probe = {answer_probe}\n{gate}")
}

/// One worker-backed registry with the stand-in agent installed as a profile.
struct Fixture {
    env: TestEnv,
    registry: SessionRegistry,
    scratch: PathBuf,
}

impl Fixture {
    async fn new(answer_probe: bool) -> Self {
        Self::gated(answer_probe, None).await
    }

    /// A fixture whose manifest declares `authority_from`.
    async fn gated(answer_probe: bool, authority_from: Option<&str>) -> Self {
        Self::hosting(answer_probe, authority_from, |_env, _scratch| {
            ShellCommand::new("/bin/sh", ["-c", "sleep 600"])
        })
        .await
    }

    /// A fixture whose `shell` sessions run the command `shell` builds from the
    /// test environment and the scratch directory.
    async fn hosting(
        answer_probe: bool,
        authority_from: Option<&str>,
        shell: impl FnOnce(&TestEnv, &Path) -> ShellCommand,
    ) -> Self {
        let env = TestEnv::new().expect("create the test environment");
        let scratch = env.cwd().join("scratch");
        std::fs::create_dir(&scratch).expect("create the scratch directory");
        let shell_command = shell(&env, &scratch);
        let agents = env.root().join("agents");
        std::fs::create_dir_all(agents.join("manifests")).expect("create the agents directory");
        std::fs::write(
            agents.join("manifests/fake.toml"),
            opted_in_manifest(answer_probe, authority_from),
        )
        .expect("write the override manifest");
        let program = env.root().join("fake-agent");
        std::fs::write(&program, agent_script()).expect("write the stand-in agent");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in agent executable");
        for (name, mode, source) in [
            ("fake-reply-stdin", "reply", "stdin"),
            ("fake-reply-fifo", "reply", "fifo"),
            ("fake-silent", "silent", "stdin"),
            ("fake-recording", "recording", "stdin"),
        ] {
            std::fs::write(
                agents.join(format!("{name}.toml")),
                format!(
                    "base = \"claude\"\n{}program = \"{}\"\nargs = [\"{mode}\", \"{}\", \"{source}\"]\nmanifest = \"fake\"\n\n[env]\nPATH = \"/usr/bin:/bin\"\n",
                    official_runtimes::profile_pin("claude"),
                    program.display(),
                    scratch.display()
                ),
            )
            .expect("write the profile");
        }
        let fifo = scratch.join("cmd");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("run mkfifo")
                .success(),
            "create the command FIFO"
        );

        let catalog = official_runtimes::OfficialCatalog::new();
        let worker_environment = SubprocessWorkerEnvironment {
            runtime_home: env.runtime_dir().to_path_buf(),
            state_home: env.state_home().to_path_buf(),
            data_home: env.data_home().to_path_buf(),
            config_home: env.config_home().to_path_buf(),
            cache_home: env.cache_home().to_path_buf(),
            home: env.home().to_path_buf(),
            daemon_socket: env.root().join("daemon.sock"),
        };
        let mut config = catalog.configure(
            SessionRegistryConfig {
                shell_command,
                procwatch_poll: PROCWATCH_POLL,
                agents_dir: Some(agents),
                host_state_dir: Some(env.root().join("host-state")),
                ..SessionRegistryConfig::default()
            },
            env.state_home().join("pohunek/plugins"),
        );
        config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
        config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
        config.supervision = Some(
            worker_environment
                .supervision(worker_binary())
                .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
        );
        let registry = SessionRegistry::new_production_with_inspector(
            config,
            Arc::new(SubprocessWorkerLauncher::new()),
            Arc::new(HostInspector::new()),
        )
        .expect("the production registry opens the plugin root");
        catalog.install(&registry, env.root()).await;
        Self {
            env,
            registry,
            scratch,
        }
    }

    /// Starts the stand-in agent of the host profile `profile`.
    async fn start(&self, profile: &str) -> SessionInfo {
        let mut process_env = ProcessEnv::lock();
        process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
        self.registry
            .create(SessionNewParams {
                client_request_id: None,
                name: None,
                agent: profile.to_owned(),
                cwd: Some(self.env.cwd().to_path_buf()),
                cols: TEST_COLS,
                rows: TEST_ROWS,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                branch_source: None,
                on_diverged: None,
                input_ack_timeout_ms: None,
                metadata: BTreeMap::new(),
                secret_env: protocol::SessionSecretEnv::default(),
            })
            .await
            .expect("create the stand-in agent session")
    }

    async fn activity(&self, id: &SessionId) -> Option<(AgentActivity, StateSource)> {
        let info = self.registry.inspect(id).await.expect("inspect session");
        info.activity.map(|activity| (activity, info.state_source))
    }

    async fn wait_activity(&self, id: &SessionId, expected: (AgentActivity, StateSource)) {
        wait_until(&format!("the session to read {expected:?}"), || async {
            (self.activity(id).await == Some(expected)).then_some(())
        })
        .await;
    }

    async fn input(
        &self,
        id: &SessionId,
        text: &str,
        until: AgentActivity,
    ) -> Result<protocol::SessionInputResult, protocol::ProtocolError> {
        self.registry
            .input(SessionInputParams {
                session_id: id.clone(),
                text: text.to_owned(),
                wait: Some(SessionInputWait {
                    until: Some(vec![until]),
                    timeout_ms: Some(WAIT_MS),
                    allow_prompt_accepted: None,
                }),
            })
            .await
    }

    fn scratch_file(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Writes one command byte to the FIFO the stand-in agent reads.
fn command(fixture: &Fixture, byte: u8) {
    std::fs::write(fixture.scratch_file("cmd"), [byte]).expect("write the command byte");
}

#[tokio::test]
async fn a_probe_waiting_agent_reports_activity_through_program_status() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new(true).await;
    let created = fixture.start("fake-reply-stdin").await;

    // The worker answers the probe exactly once, and the agent then reports idle.
    let reply = wait_until("the agent to record the worker's reply", || async {
        read_trimmed(&fixture.scratch_file("reply.hex"))
    })
    .await;
    assert_eq!(reply, REPLY_HEX);
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;

    let detection = fixture
        .registry
        .detection(&created.id)
        .await
        .expect("session.detection");
    // A client that predates the program-status kind decodes the result: the
    // record is not on the wire.
    let wire = serde_json::to_value(&detection).expect("serialize session.detection");
    let legacy: LegacyDetection =
        serde_json::from_value(wire).expect("an older client decodes session.detection");
    assert_eq!(legacy.supported_regions.len(), 10);
    assert!(!legacy.previews.is_empty());

    let flow = async {
        // A waited input that the agent answers with working evidence.
        let working = fixture
            .input(&created.id, "t", AgentActivity::Working)
            .await
            .expect("input until working");
        assert_eq!(working.activity, Some(AgentActivity::Working));
        assert_eq!(working.activity_source, Some(StateSource::OscProgress));
        command(&fixture, b'd');
        fixture
            .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
            .await;

        // A waited input that must see working evidence and then idle evidence.
        let release = async {
            fixture
                .wait_activity(
                    &created.id,
                    (AgentActivity::Working, StateSource::OscProgress),
                )
                .await;
            command(&fixture, b'd');
        };
        let idle = fixture.input(&created.id, "t", AgentActivity::Idle);
        let (idle, ()) = tokio::join!(idle, release);
        let idle = idle.expect("input until idle");
        assert_eq!(idle.activity, Some(AgentActivity::Idle));
        assert_eq!(idle.activity_source, Some(StateSource::OscProgress));
    };
    flow.await;
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// A tracing writer that keeps what the daemon logs.
#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_profile_with_fork_args_starts_and_logs_that_they_are_ignored() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let captured = CapturedLog::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish(),
    )
    .expect("no other subscriber is installed in this test process");
    let fixture = Fixture::new(true).await;
    let program = fixture.env.root().join("fake-agent");
    std::fs::write(
        fixture.env.root().join("agents/fake-fork-args.toml"),
        format!(
            "base = \"claude\"\n{}program = \"{}\"\nargs = [\"silent\", \"{}\", \"stdin\"]\nmanifest = \"fake\"\n\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume\", \"{{reference}}\"]\nfork_args = [\"--resume\", \"{{reference}}\", \"--fork-session\"]\n\n[env]\nPATH = \"/usr/bin:/bin\"\n",
            official_runtimes::profile_pin("claude"),
            program.display(),
            fixture.scratch.display()
        ),
    )
    .expect("write the profile");

    let created = fixture.start("fake-fork-args").await;
    let logged = String::from_utf8_lossy(&captured.0.lock().expect("log buffer")).into_owned();
    assert!(
        logged.contains("ignoring resume.fork_args in the host profile"),
        "{logged}"
    );
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

#[tokio::test]
async fn a_record_without_input_changes_the_activity() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new(true).await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;

    command(&fixture, b'g');
    fixture
        .wait_activity(
            &created.id,
            (AgentActivity::Working, StateSource::OscProgress),
        )
        .await;
    command(&fixture, b'd');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;

    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

#[tokio::test]
async fn a_blocked_record_is_blocked_activity_and_refuses_a_waited_input() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new(true).await;
    let created = fixture.start("fake-reply-stdin").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;

    let blocked = fixture
        .input(&created.id, "b", AgentActivity::Blocked)
        .await
        .expect("input until blocked");
    assert_eq!(blocked.activity, Some(AgentActivity::Blocked));
    assert_eq!(blocked.activity_source, Some(StateSource::OscProgress));

    let refused = fixture
        .input(&created.id, "g", AgentActivity::Working)
        .await
        .expect_err("a blocked agent refuses a waited input");
    assert_eq!(refused.code, "session_agent_blocked");
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

#[tokio::test]
async fn an_agent_that_gets_no_probe_answer_is_classified_from_the_screen() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    // The manifest opts in to records but not to the worker's answer, so the
    // program never sees a reply, as under tmux or at an older release.
    let fixture = Fixture::new(false).await;
    let created = fixture.start("fake-silent").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::Screen))
        .await;

    fixture
        .registry
        .input(SessionInputParams {
            session_id: created.id.clone(),
            text: "k".to_owned(),
            wait: None,
        })
        .await
        .expect("type a key");
    let typed = wait_until("the agent to record its first input bytes", || async {
        read_trimmed(&fixture.scratch_file("typed.hex"))
    })
    .await;

    assert!(
        !typed.starts_with(&REPLY_HEX[..4]),
        "the worker writes no reply, so the first bytes are the typed key's framing: {typed}"
    );
    assert_eq!(
        fixture.activity(&created.id).await,
        Some((AgentActivity::Idle, StateSource::Screen))
    );
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

#[tokio::test]
async fn a_restated_record_emits_no_agent_state() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new(true).await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;
    command(&fixture, b'g');
    fixture
        .wait_activity(
            &created.id,
            (AgentActivity::Working, StateSource::OscProgress),
        )
        .await;
    let mut events = fixture.registry.subscribe();

    for _ in 0..5 {
        command(&fixture, b'g');
    }
    // A command the agent answers visibly after the restatements orders them
    // before the awaited edge.
    command(&fixture, b'd');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;

    let mut states = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.event() == protocol::event::AGENT_STATE {
            states.push(event);
        }
    }
    assert_eq!(
        states.len(),
        1,
        "the restated working emits nothing; only the edge to idle does: {states:?}"
    );
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// The child records of a Claude background agent never change what the
/// session reads: they do not turn a held root `done` working. A child record
/// that dropped the held root record is not observed here, because the idle
/// reading the screen would then give is debounced and the held record is not
/// on the wire; `a_child_record_never_changes_the_root_record` in
/// `program_status_detector.rs` guards that direction at the detector.
#[tokio::test]
async fn a_background_agents_child_records_never_change_the_session_activity() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new(true).await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;
    command(&fixture, b'g');
    fixture
        .wait_activity(
            &created.id,
            (AgentActivity::Working, StateSource::OscProgress),
        )
        .await;

    // The child records add no edge between the root `working` and `done`.
    let mut events = fixture.registry.subscribe();
    command(&fixture, b'c');
    command(&fixture, b'x');
    command(&fixture, b'd');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;
    assert_eq!(
        agent_states(&mut events).len(),
        1,
        "only the root `done` changes the activity"
    );

    // The detector task answers `session.detection` from its own screen, so
    // once the marker shows there, it has consumed the chunk with the child
    // records. The root `blocked` record is written only then, so it cannot
    // share a chunk with them, and a child record read as a root `working`
    // would have published an edge before the awaited `blocked`.
    command(&fixture, b'c');
    command(&fixture, b'x');
    command(&fixture, b'm');
    wait_until("the detector to see the marker line", || async {
        let detection = fixture.registry.detection(&created.id).await.ok()?;
        detection
            .previews
            .iter()
            .any(|preview| preview.text.contains("SYNC"))
            .then_some(())
    })
    .await;
    command(&fixture, b'b');
    fixture
        .wait_activity(
            &created.id,
            (AgentActivity::Blocked, StateSource::OscProgress),
        )
        .await;
    assert_eq!(
        agent_states(&mut events).len(),
        1,
        "the child records over a held root `done` add no edge before the root `blocked`"
    );
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// The `agent.state` events received so far.
fn agent_states(
    events: &mut tokio::sync::broadcast::Receiver<protocol::Event>,
) -> Vec<protocol::Event> {
    let mut states = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.event() == protocol::event::AGENT_STATE {
            states.push(event);
        }
    }
    states
}

#[tokio::test]
async fn a_release_below_authority_from_is_screen_only_and_gets_no_probe_answer() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    // The stand-in reports the locked Claude release, far below the gate.
    let fixture = Fixture::gated(true, Some("99.0.0")).await;
    let created = fixture.start("fake-recording").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::Screen))
        .await;
    fixture
        .registry
        .input(SessionInputParams {
            session_id: created.id.clone(),
            text: "g".to_owned(),
            wait: None,
        })
        .await
        .expect("type a key");
    let typed = wait_until("the agent to record its first input bytes", || async {
        read_trimmed(&fixture.scratch_file("typed.hex"))
    })
    .await;
    assert!(
        !typed.starts_with(&REPLY_HEX[..4]),
        "the closed gate writes no probe reply: {typed}"
    );
    // The detector answers `session.detection` from its own screen, so once the
    // marker shows there it has consumed the chunk with the working record.
    wait_until("the detector to see the marker line", || async {
        let detection = fixture.registry.detection(&created.id).await.ok()?;
        detection
            .previews
            .iter()
            .any(|preview| preview.text.contains("SYNC"))
            .then_some(())
    })
    .await;
    assert_eq!(
        fixture.activity(&created.id).await,
        Some((AgentActivity::Idle, StateSource::Screen)),
        "a working record below the gate is ignored"
    );
    let status = fixture
        .registry
        .detection(&created.id)
        .await
        .expect("session.detection")
        .status
        .expect("the daemon reports the detection status");
    assert_eq!(status.state_source, Some(StateSource::Screen));
    assert!(!status.probe_answered);
    assert!(!status.record_present);
    assert!(status.gate.declared && !status.gate.open);
    assert_eq!(status.gate.reason, protocol::AuthorityGateReason::Below);
    assert_eq!(status.gate.authority_from.as_deref(), Some("99.0.0"));
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

#[tokio::test]
async fn a_release_at_or_above_authority_from_answers_the_probe_and_reads_records() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::gated(true, Some("0.0.1")).await;
    let created = fixture.start("fake-reply-stdin").await;
    let reply = wait_until("the agent to record the worker's reply", || async {
        read_trimmed(&fixture.scratch_file("reply.hex"))
    })
    .await;
    assert_eq!(reply, REPLY_HEX);
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;
    let status = fixture
        .registry
        .detection(&created.id)
        .await
        .expect("session.detection")
        .status
        .expect("the daemon reports the detection status");
    assert_eq!(status.state_source, Some(StateSource::OscProgress));
    assert!(status.probe_answered);
    assert!(status.record_present);
    assert!(status.gate.declared && status.gate.open);
    assert_eq!(status.gate.reason, protocol::AuthorityGateReason::AtOrAbove);
    assert_eq!(status.gate.authority_from.as_deref(), Some("0.0.1"));
    assert!(status.gate.runtime_version.is_some());
    assert!(status.last_evidence_age_ms.is_some());
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// Name of the stand-in the nested scenarios run: a copy of the shell whose
/// kernel task name is the official Claude package's.
const NESTED_CLAUDE: &str = "claude";
/// Name of a stand-in no installed runtime recognises.
const NESTED_UNLISTED: &str = "unlisted-agent";

/// The probing program of the nested scenarios: it prints the probe, records
/// the first bytes that reach it, and then reports the record the byte `g`,
/// `b` or `d` asks for.
const NESTED_PROBER: &str = r#"
stty raw -echo
printf '\033]7501;?\033\\'
head -c "$1" | od -An -v -tx1 | tr -d ' \n' > "$2/first.hex"
emit() { printf '\033]7501;state=%s:app=claude-code\033\\' "$1"; }
emit idle
while :; do
  c=$(head -c 1)
  case "$c" in
    g) emit working ;;
    b) emit 'blocked:kind=permission' ;;
    d) emit done ;;
  esac
done
"#;

/// A shell session that starts `program` below its root process.
async fn nested_fixture(program: &'static str, first_bytes: usize) -> Fixture {
    Fixture::hosting(true, None, move |env, scratch| {
        let bin = env.root().join("nested-bin");
        std::fs::create_dir_all(&bin).expect("create the stand-in directory");
        let stand_in = bin.join(program);
        std::fs::copy("/bin/sh", &stand_in).expect("copy the shell as the stand-in");
        let script = bin.join("prober.sh");
        std::fs::write(&script, NESTED_PROBER).expect("write the probing program");
        let command = format!(
            "{} {} {first_bytes} {}; sleep 600",
            stand_in.display(),
            script.display(),
            scratch.display()
        );
        ShellCommand::new("/bin/sh", ["-c", command.as_str()])
    })
    .await
}

/// A Claude started below a shell session gets the worker's reply to its probe
/// and its records drive the session's activity, though the shell session's own
/// runtime answers no probe.
#[tokio::test]
async fn a_claude_started_below_a_shell_is_answered_and_its_records_drive_the_activity() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = nested_fixture(NESTED_CLAUDE, REPLY_BYTES).await;
    let created = fixture.start("shell").await;

    let reply = wait_until("the nested Claude to record the worker's reply", || async {
        read_trimmed(&fixture.scratch_file("first.hex"))
    })
    .await;
    assert_eq!(reply, REPLY_HEX);
    // The records the program sent before the process scan recognised it were
    // dropped with the shell's detector; the ones after the swap are held.
    wait_until(
        "the process scan to recognise the nested Claude",
        || async {
            let info = fixture.registry.inspect(&created.id).await.ok()?;
            (info.active_agent.as_deref() == Some("claude")).then_some(())
        },
    )
    .await;
    for (byte, expected) in [
        (b'g', AgentActivity::Working),
        (b'b', AgentActivity::Blocked),
        (b'd', AgentActivity::Idle),
    ] {
        fixture
            .registry
            .input(SessionInputParams {
                session_id: created.id.clone(),
                text: String::from(char::from(byte)),
                wait: None,
            })
            .await
            .expect("type a command byte");
        fixture
            .wait_activity(&created.id, (expected, StateSource::OscProgress))
            .await;
    }
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// A program no installed runtime recognises gets no reply to its probe, so
/// the first bytes it reads are the ones typed at it.
#[tokio::test]
async fn an_unlisted_program_started_below_a_shell_gets_no_probe_reply() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = nested_fixture(NESTED_UNLISTED, 1).await;
    let created = fixture.start("shell").await;

    fixture
        .registry
        .input(SessionInputParams {
            session_id: created.id.clone(),
            text: "x".to_owned(),
            wait: None,
        })
        .await
        .expect("type a byte");
    let first = wait_until("the program to record its first byte", || async {
        read_trimmed(&fixture.scratch_file("first.hex"))
    })
    .await;
    assert_eq!(first, "78", "the first byte is the typed `x`, not a reply");
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}
