//! OSC 7501 program status through a real session: worker, PTY and registry.
//!
//! The official packages do not opt in yet, so the sessions here run a host
//! profile over the official Claude package whose override manifest is the
//! shipped Claude manifest plus a `[program_status]` table. The program is a
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
    SessionTurnEventKind, SessionWaitTurnParams, SessionWaitTurnReason, StateSource,
    ENV_DAEMON_ID, ENV_SESSION_ID,
};

const TEST_COLS: u16 = 100;
const TEST_ROWS: u16 = 24;
/// The `app` the stand-in agent's records carry and its manifest opts in with.
const APP: &str = "fake";
/// The reply the worker writes for an answered probe, as hex.
const REPLY_HEX: &str = "1b5d373530313b3f1b5c";
/// Bound of a wait that must be satisfied by the stand-in agent.
const WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;
/// Bound of a wait that must end without the awaited event.
const ABSENCE_WAIT_MS: u32 = 1_500;

/// The stand-in agent. `$1` is the mode, `$2` the scratch directory and `$3`
/// where commands come from.
///
/// It answers the version probe, switches the terminal to raw mode, prints the
/// probe and, in the `reply` mode, records the first ten bytes it reads (the
/// worker's reply) before it draws a Claude prompt box. A command read from the
/// terminal (`stdin` source) runs on Enter, as a submitted prompt does; one
/// read from the `cmd` FIFO (`fifo` source) runs at once. The commands: `g`
/// reports working, `d` done, `b` blocked and `q` exits; `t` is a turn that
/// reports working until the FIFO yields a byte, then done.
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
printf '\033[2J\033[H'
printf '────────────────────────────────────────\r\n'
printf '❯ \r\n'
printf '────────────────────────────────────────\r\n'
printf '  ? for shortcuts'
if [ "$mode" = reply ]; then emit idle; fi
if [ "$mode" = silent ]; then
  head -c 2 | od -An -v -tx1 | tr -d ' \n' > "$dir/typed.hex"
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

fn opted_in_manifest(answer_probe: bool) -> String {
    let shipped = std::fs::read_to_string(workspace_root().join("runtime-packages/claude/detect.toml"))
        .expect("read the shipped Claude manifest");
    format!("{shipped}\n[program_status]\napp = \"{APP}\"\nanswer_probe = {answer_probe}\n")
}

/// One worker-backed registry with the stand-in agent installed as a profile.
struct Fixture {
    env: TestEnv,
    registry: SessionRegistry,
    scratch: PathBuf,
}

impl Fixture {
    async fn new(answer_probe: bool) -> Self {
        let env = TestEnv::new().expect("create the test environment");
        let scratch = env.cwd().join("scratch");
        std::fs::create_dir(&scratch).expect("create the scratch directory");
        let agents = env.root().join("agents");
        std::fs::create_dir_all(agents.join("manifests")).expect("create the agents directory");
        std::fs::write(
            agents.join("manifests/fake.toml"),
            opted_in_manifest(answer_probe),
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
                shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 600"]),
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
            })
            .await
            .expect("create the stand-in agent session")
    }

    async fn activity(&self, id: &SessionId) -> Option<(AgentActivity, StateSource)> {
        let info = self.registry.inspect(id).await.expect("inspect session");
        info.activity.map(|activity| (activity, info.state_source))
    }

    async fn wait_activity(&self, id: &SessionId, expected: (AgentActivity, StateSource)) {
        wait_until(
            &format!("the session to read {expected:?}"),
            || async { (self.activity(id).await == Some(expected)).then_some(()) },
        )
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

async fn turn_wait(
    fixture: &Fixture,
    id: &SessionId,
    after: Option<protocol::SessionTurnCursor>,
    timeout_ms: u32,
) -> protocol::SessionWaitTurnResult {
    let params = SessionWaitTurnParams::new(
        vec![id.clone()],
        vec![SessionTurnEventKind::TurnCompleted],
        after,
        Some(timeout_ms),
    )
    .expect("turn wait parameters");
    fixture
        .registry
        .wait_turn(&params)
        .await
        .expect("wait for a turn event")
}

#[tokio::test]
async fn a_probe_waiting_agent_reports_turns_through_program_status() {
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
    assert!(detection
        .previews
        .iter()
        .any(|preview| preview.kind == protocol::DetectionRegionKind::ProgramStatus
            && preview.text == format!("app={APP}:state=idle")));

    let baseline = turn_wait(&fixture, &created.id, None, 1).await.cursor;
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
    let turn = turn_wait(&fixture, &created.id, Some(baseline), WAIT_MS).await;

    assert_eq!(turn.reason, SessionWaitTurnReason::Event);
    assert_eq!(
        turn.event.map(|event| event.kind),
        Some(SessionTurnEventKind::TurnCompleted)
    );
    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_record_without_input_changes_the_activity_and_opens_no_turn() {
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
        .wait_activity(&created.id, (AgentActivity::Working, StateSource::OscProgress))
        .await;
    command(&fixture, b'd');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscProgress))
        .await;

    let turn = turn_wait(&fixture, &created.id, None, ABSENCE_WAIT_MS).await;
    assert_eq!(
        turn.reason,
        SessionWaitTurnReason::Timeout,
        "a record never arms the input token, so no turn completes"
    );
    let _ = fixture.registry.stop(&created.id).await;
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
    let _ = fixture.registry.stop(&created.id).await;
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
    let _ = fixture.registry.stop(&created.id).await;
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
        .wait_activity(&created.id, (AgentActivity::Working, StateSource::OscProgress))
        .await;
    let revision = fixture
        .registry
        .inspect(&created.id)
        .await
        .expect("inspect session")
        .updated_at;
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
    assert!(revision <= fixture.registry.inspect(&created.id).await.expect("inspect").updated_at);
    let _ = fixture.registry.stop(&created.id).await;
}
