//! The official Pi package's OSC 7501 opt-in through a real session: worker,
//! PTY, registry and the shipped Pi manifest.
//!
//! The program is a POSIX shell stand-in for Pi. It draws the captured Pi
//! screens of `compat/pi/screens`, prints Pi's 7501 probe, waits for the
//! worker's reply and reports its state by raw `app=pi` records, as Pi 1.1.0
//! and later do. The `old` mode models a Pi release without program status: it
//! prints no probe and no record.

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

const TEST_COLS: u16 = 100;
const TEST_ROWS: u16 = 24;
/// The reply the worker writes for an answered probe, as hex.
const REPLY_HEX: &str = "1b5d373530313b3f1b5c";
/// Bound of a wait that must be satisfied by the stand-in agent.
const WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;
/// Captured Pi screens the stand-in draws, relative to `compat/pi/screens`.
const IDLE_SCREEN: &str = "idle.txt";
const WORKING_SCREEN: &str = "working.txt";
const OWN_CONFIRM_SCREEN: &str = "overlays/blocked_project_trust_w100.txt";
const UNFRAMED_SCREEN: &str = "overlays/unframed_fork_other_project_w100.txt";

/// The stand-in agent. `$1` is the mode, `$2` the scratch directory, `$3`
/// where commands come from and `$4` the screens directory.
///
/// In the `reply` mode it prints the probe and records the first ten bytes it
/// reads (the worker's reply) before it draws the idle screen and reports
/// idle; in the `old` mode it draws the idle screen and nothing else. A
/// command read from the terminal (`stdin` source) runs on Enter, as a
/// submitted prompt does, with Pi's bracketed-paste framing ignored; one read from the `cmd` FIFO (`fifo` source) runs at
/// once. Commands: `g` working screen and record, `d` idle screen and done
/// record, `b` blocked record, `c` Pi's own confirm over the screen, `s` the
/// busy border with an idle record (a branch summary), `x` a screen no rule
/// matches, `r` the idle screen, `t` a turn that reports working until the
/// FIFO yields a byte and then done, and `l` the clear record. In the `old`
/// mode the screens change and no record is written.
fn agent_script() -> String {
    format!(
        r#"#!/bin/sh
{version}
mode=$1; dir=$2; source=$3; screens=$4
stty raw -echo
draw() {{
  printf '\033[2J\033[H'
  awk 'NR>1{{printf "\r\n"}} {{printf "%s",$0}}' "$screens/$1"
}}
emit() {{
  if [ "$mode" = reply ]; then printf '\033]7501;state=%s:app=pi\033\\' "$1"; fi
}}
if [ "$mode" = reply ]; then
  printf '\033]7501;?\033\\'
  head -c 10 | od -An -v -tx1 | tr -d ' \n' > "$dir/reply.hex"
fi
draw {idle}
emit idle
exec 3<> "$dir/cmd"
cr=$(printf '\r')
pending=
run() {{
  case "$1" in
    g) draw {working}; emit working ;;
    d) draw {idle}; emit done ;;
    b) emit 'blocked:kind=permission' ;;
    c) draw {own_confirm} ;;
    s) draw {working}; emit idle ;;
    x) draw {unframed} ;;
    r) draw {idle} ;;
    t) draw {working}; emit working; head -c 1 <&3 > /dev/null; draw {idle}; emit done ;;
    l) if [ "$mode" = reply ]; then printf '\033]7501;state=clear\033\\'; fi ;;
  esac
}}
while :; do
  if [ "$source" = fifo ]; then
    run "$(head -c 1 <&3)"
  else
    c=$(head -c 1)
    if [ "$c" = "$cr" ]; then
      run "$(printf '%s' "$pending" | tr -d '\033[~0-9')"; pending=
    else pending=$pending$c; fi
  fi
done
"#,
        version = official_runtimes::sh_version_answer("pi"),
        idle = IDLE_SCREEN,
        working = WORKING_SCREEN,
        own_confirm = OWN_CONFIRM_SCREEN,
        unframed = UNFRAMED_SCREEN,
    )
}

/// One worker-backed registry with the stand-in agent installed as profiles
/// over the official Pi package.
struct Fixture {
    env: TestEnv,
    registry: SessionRegistry,
    scratch: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let env = TestEnv::new().expect("create the test environment");
        let scratch = env.cwd().join("scratch");
        std::fs::create_dir(&scratch).expect("create the scratch directory");
        let agents = env.root().join("agents");
        std::fs::create_dir_all(&agents).expect("create the agents directory");
        let program = env.root().join("fake-agent");
        std::fs::write(&program, agent_script()).expect("write the stand-in agent");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in agent executable");
        let screens = workspace_root().join("compat/pi/screens");
        for (name, mode, source) in [
            ("fake-reply-stdin", "reply", "stdin"),
            ("fake-reply-fifo", "reply", "fifo"),
            ("fake-old-fifo", "old", "fifo"),
        ] {
            std::fs::write(
                agents.join(format!("{name}.toml")),
                format!(
                    "base = \"pi\"\n{}program = \"{}\"\nargs = [\"{mode}\", \"{}\", \"{source}\", \"{}\"]\n\n[env]\nPATH = \"/usr/bin:/bin\"\n",
                    official_runtimes::profile_pin("pi"),
                    program.display(),
                    scratch.display(),
                    screens.display(),
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

const WORKING: (AgentActivity, StateSource) = (AgentActivity::Working, StateSource::OscProgress);
const IDLE: (AgentActivity, StateSource) = (AgentActivity::Idle, StateSource::OscProgress);

#[tokio::test]
async fn the_shipped_pi_package_opts_in_and_the_worker_answers_its_probe_once() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;

    let reply = wait_until("the agent to record the worker's reply", || async {
        read_trimmed(&fixture.scratch_file("reply.hex"))
    })
    .await;
    assert_eq!(reply, REPLY_HEX, "the reply is the fixed probe reply");
    fixture.wait_activity(&created.id, IDLE).await;

    let detection = fixture
        .registry
        .detection(&created.id)
        .await
        .expect("session.detection");
    assert!(
        detection.previews.iter().any(|preview| preview.kind
            == protocol::DetectionRegionKind::ProgramStatus
            && preview.text == "app=pi:state=idle"),
        "the shipped manifest declares app = pi: {detection:?}"
    );

    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_pi_turn_is_working_then_idle_by_record() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;

    let working = fixture
        .input(&created.id, "t", AgentActivity::Working)
        .await
        .expect("input until working");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    assert_eq!(working.activity_source, Some(StateSource::OscProgress));
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    // Idle evidence needs working evidence first.
    let release = async {
        fixture.wait_activity(&created.id, WORKING).await;
        command(&fixture, b'd');
    };
    let idle = fixture.input(&created.id, "t", AgentActivity::Idle);
    let (idle, ()) = tokio::join!(idle, release);
    let idle = idle.expect("input until idle");
    assert_eq!(idle.activity, Some(AgentActivity::Idle));
    assert_eq!(idle.activity_source, Some(StateSource::OscProgress));
    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_pi_confirm_record_is_blocked_and_pis_own_confirm_stays_blocked_by_screen() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;

    // An extension dialog: Pi reports blocked.
    command(&fixture, b'b');
    fixture
        .wait_activity(
            &created.id,
            (AgentActivity::Blocked, StateSource::OscProgress),
        )
        .await;
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;

    // Pi's own confirm is drawn while the record says working: the visible
    // dialog wins.
    command(&fixture, b'c');
    fixture
        .wait_activity(&created.id, (AgentActivity::Blocked, StateSource::Screen))
        .await;
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;
    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_pi_busy_border_with_an_idle_record_reads_idle_and_an_unmatched_screen_keeps_working() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;

    // A branch summary draws the busy border without a run: Pi's record is idle.
    command(&fixture, b's');
    wait_until("the busy border to be drawn", || async {
        fixture
            .registry
            .screen(&created.id)
            .await
            .ok()
            .filter(|screen| {
                screen
                    .visible_lines
                    .iter()
                    .any(|line| line.contains("Working"))
            })
    })
    .await;
    assert_eq!(fixture.activity(&created.id).await, Some(IDLE));

    // A dialog drawn with ctx.ui.custom() matches no screen rule: the record
    // decides.
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;
    command(&fixture, b'x');
    wait_until("the unframed screen to be drawn", || async {
        fixture
            .registry
            .screen(&created.id)
            .await
            .ok()
            .filter(|screen| {
                !screen
                    .visible_lines
                    .iter()
                    .any(|line| line.contains("Working"))
            })
    })
    .await;
    assert_eq!(fixture.activity(&created.id).await, Some(WORKING));
    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_pi_clear_record_hands_detection_back_to_the_screen() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;
    command(&fixture, b's');
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;
    command(&fixture, b'r');
    // The idle screen cannot undo the held working record.
    command(&fixture, b'l');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::Screen))
        .await;
    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_pi_release_without_program_status_is_classified_from_the_screen() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-old-fifo").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::Screen))
        .await;

    command(&fixture, b'g');
    fixture
        .wait_activity(&created.id, (AgentActivity::Working, StateSource::Screen))
        .await;
    command(&fixture, b'c');
    fixture
        .wait_activity(&created.id, (AgentActivity::Blocked, StateSource::Screen))
        .await;
    command(&fixture, b'd');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::Screen))
        .await;
    assert!(
        !fixture.scratch_file("reply.hex").exists(),
        "an agent that sent no probe got no reply"
    );
    let _ = fixture.registry.stop(&created.id).await;
}
