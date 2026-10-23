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

use pohunek_daemon::notifications::{
    AttentionCoordinator, AttentionCoordinatorTask, NotificationProjector, NotificationService,
};
use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{
    RedeemedRuntime, SessionRegistry, SessionRegistryConfig, ShellCommand,
};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{worker_binary, workspace_root};
use pohunek_worker_protocol::{write_frame, DataFrame, FrameHeader, FrameKind, WriteId};
use protocol::{
    AgentActivity, SessionAttachParams, SessionId, SessionInfo, SessionInputParams,
    SessionInputWait, SessionNewParams, SessionTurnEventKind, SessionWaitTurnParams,
    SessionWaitTurnReason, StateSource, ENV_DAEMON_ID, ENV_SESSION_ID,
};

const TEST_COLS: u16 = 100;
const TEST_ROWS: u16 = 24;
/// The reply the worker writes for an answered probe, as hex.
const REPLY_HEX: &str = "1b5d373530313b3f1b5c";
/// Bound of a wait that must be satisfied by the stand-in agent.
const WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;
/// Turn waits: a turn event follows the attention debounce (5 seconds by
/// default), so the absence of one is only proved by a wait that outlasts it.
// timing-allowed: #1525 no later event marks the end of the debounce window; the wait is the only evidence of an absent turn event
const NO_TURN_WAIT_MS: u32 = 8_000;
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
/// submitted prompt does, with Pi's bracketed-paste framing ignored; one read
/// from the `cmd` FIFO (`fifo` source) runs at once. With the FIFO source the
/// agent also records any further input bytes that arrive within a second of
/// the reply (`extra.hex`), so a second reply would show.
///
/// Commands: `g` working screen and record, `d` idle screen and done record,
/// `b` blocked record, `c` Pi's own confirm over the screen, `s` the busy
/// border with an idle record (a branch summary), `x` a screen no rule matches,
/// `r` the idle screen, `w` a working and a done record in one write, `t` a
/// turn that reports working until the FIFO yields a byte and then done, and
/// `l` the clear record. In the `old` mode the screens change and no record is
/// written.
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
  if [ "$mode" != old ]; then printf '\033]7501;state=%s:app=pi\033\\' "$1"; fi
}}
if [ "$mode" = reply ]; then
  printf '\033]7501;?\033\\'
  head -c 10 | od -An -v -tx1 | tr -d ' \n' > "$dir/reply.hex"
  if [ "$source" = fifo ]; then
    timeout 1 head -c 10 | od -An -v -tx1 | tr -d ' \n' > "$dir/extra.hex"
  fi
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
    w) if [ "$mode" = reply ]; then printf '\033]7501;state=working:app=pi\033\\\033]7501;state=done:app=pi\033\\'; fi ;;
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

/// Writes the ES4 profile `fake-shell-es4` and the files it runs in `scratch`.
fn write_nested_pi_profile(agents: &Path, scratch: &Path) {
    // ES4: a shell session whose foreground process is a `pi`, so procwatch
    // switches the session to the Pi manifest. The nested `pi` waits on
    // `go1`, reports `working`, waits on `go2` and exits.
    let pi_link = scratch.join("pi");
    std::os::unix::fs::symlink("/bin/sh", &pi_link).expect("link the nested pi");
    let nested = scratch.join("nested.sh");
    std::fs::write(
        &nested,
        "read _ < go1\nprintf '\\033]7501;state=working:app=pi\\033\\\\'\nread _ < go2\n",
    )
    .expect("write the nested pi script");
    let outer = scratch.join("outer.sh");
    std::fs::write(
        &outer,
        format!(
            "cd {dir}\n{pi} {nested} &\nwait $!\nprintf ready\nsleep 600\n",
            dir = scratch.display(),
            pi = pi_link.display(),
            nested = nested.display()
        ),
    )
    .expect("write the outer shell script");
    std::fs::write(
        agents.join("fake-shell-es4.toml"),
        format!(
            "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"{}\"]\n\n[env]\nPATH = \"/usr/bin:/bin\"\n",
            outer.display()
        ),
    )
    .expect("write the ES4 profile");
    for gate in ["go1", "go2"] {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(scratch.join(gate))
                .status()
                .expect("run mkfifo")
                .success(),
            "create the {gate} FIFO"
        );
    }
}

/// One worker-backed registry with the stand-in agent installed as profiles
/// over the official Pi package.
struct Fixture {
    env: TestEnv,
    registry: SessionRegistry,
    scratch: PathBuf,
    projector: NotificationProjector,
    _attention: AttentionCoordinatorTask,
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
            ("fake-nested-fifo", "nested", "fifo"),
        ] {
            // The nested stand-in runs under a shell session, whose manifest
            // has no `[program_status]` table.
            let (base, pin) = if mode == "nested" {
                ("shell", String::new())
            } else {
                ("pi", official_runtimes::profile_pin("pi"))
            };
            std::fs::write(
                agents.join(format!("{name}.toml")),
                format!(
                    "base = \"{base}\"\n{pin}program = \"{}\"\nargs = [\"{mode}\", \"{}\", \"{source}\", \"{}\"]\n\n[env]\nPATH = \"/usr/bin:/bin\"\n",
                    program.display(),
                    scratch.display(),
                    screens.display(),
                ),
            )
            .expect("write the profile");
        }
        write_nested_pi_profile(&agents, &scratch);
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
                procwatch_poll: std::time::Duration::from_millis(100),
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
        // The daemon's turn events come from the projector, which turns a
        // `working` to `idle` change of an armed turn into `turn_completed`.
        let notifications = NotificationService::open(&env.root().join("daemon-data"))
            .expect("open the notification inbox");
        let (coordinator, attention) =
            AttentionCoordinator::spawn(notifications.clone(), &registry);
        let projector = NotificationProjector::spawn(&registry, notifications, coordinator);
        registry.spawn_agent_state_hooks();
        Self {
            env,
            registry,
            scratch,
            projector,
            _attention: attention,
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

    /// Waits up to `timeout_ms` for a `turn_completed` event of `id` newer than
    /// `after` and returns the reason with the cursor for the next call.
    async fn wait_turn(
        &self,
        id: &SessionId,
        timeout_ms: u32,
        after: Option<protocol::SessionTurnCursor>,
    ) -> (SessionWaitTurnReason, protocol::SessionTurnCursor) {
        let params = SessionWaitTurnParams::new(
            vec![id.clone()],
            vec![SessionTurnEventKind::TurnCompleted],
            after,
            Some(timeout_ms),
        )
        .expect("valid turn wait");
        let result = self
            .registry
            .wait_turn(&params)
            .await
            .expect("session.wait_turn");
        (result.reason, result.cursor)
    }

    async fn end(self) {
        self.projector.shutdown().await;
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
const BLOCKED: (AgentActivity, StateSource) = (AgentActivity::Blocked, StateSource::OscProgress);

/// Waits until the visible screen of `id` holds a line with `needle` (or, with
/// `present` false, none).
async fn wait_screen(fixture: &Fixture, id: &SessionId, needle: &str, present: bool) {
    wait_until(
        &format!("the screen to show {needle:?}: {present}"),
        || async {
            let screen = fixture.registry.screen(id).await.ok()?;
            (screen
                .visible_lines
                .iter()
                .any(|line| line.contains(needle))
                == present)
                .then_some(())
        },
    )
    .await;
}

/// The `agent_state` events published since `events` was subscribed.
fn agent_states(events: &mut tokio::sync::broadcast::Receiver<protocol::Event>) -> usize {
    let mut count = 0;
    while let Ok(event) = events.try_recv() {
        if event.event() == protocol::event::AGENT_STATE {
            count += 1;
        }
    }
    count
}

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
    assert_eq!(
        std::fs::read_to_string(fixture.scratch_file("extra.hex")).expect("extra.hex"),
        "",
        "the probe was answered once"
    );

    // Neither the reply nor records without a delivered input open a turn.
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    let (reason, _) = fixture.wait_turn(&created.id, NO_TURN_WAIT_MS, None).await;
    assert_eq!(reason, SessionWaitTurnReason::Timeout);
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}

#[tokio::test]
async fn a_pi_turn_armed_by_input_completes_once_through_the_records() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;
    let (_, cursor) = fixture.wait_turn(&created.id, 100, None).await;

    let working = fixture
        .input(&created.id, "t", AgentActivity::Working)
        .await
        .expect("input until working");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    assert_eq!(working.activity_source, Some(StateSource::OscProgress));
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    let (reason, cursor) = fixture.wait_turn(&created.id, WAIT_MS, Some(cursor)).await;
    assert_eq!(reason, SessionWaitTurnReason::Event);
    let (reason, _) = fixture
        .wait_turn(&created.id, NO_TURN_WAIT_MS, Some(cursor))
        .await;
    assert_eq!(
        reason,
        SessionWaitTurnReason::Timeout,
        "one turn, one event"
    );

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
    fixture.end().await;
}

#[tokio::test]
async fn a_pi_turn_shorter_than_one_sample_is_latched_working() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;
    let (_, cursor) = fixture.wait_turn(&created.id, 100, None).await;

    // The working and the done record arrive in one write.
    let working = fixture
        .input(&created.id, "w", AgentActivity::Working)
        .await
        .expect("a waited input sees the working record of a one-write turn");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    fixture.wait_activity(&created.id, IDLE).await;
    let (reason, cursor) = fixture.wait_turn(&created.id, WAIT_MS, Some(cursor)).await;
    assert_eq!(reason, SessionWaitTurnReason::Event, "the turn completes");
    let (reason, _) = fixture
        .wait_turn(&created.id, NO_TURN_WAIT_MS, Some(cursor))
        .await;
    assert_eq!(reason, SessionWaitTurnReason::Timeout, "once");
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
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
    fixture.wait_activity(&created.id, BLOCKED).await;
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

    // The same holds with an idle record, as at Pi's startup questions.
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    command(&fixture, b'c');
    fixture
        .wait_activity(&created.id, (AgentActivity::Blocked, StateSource::Screen))
        .await;
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}

#[tokio::test]
async fn a_pi_busy_border_with_an_idle_record_stays_idle() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;
    let mut events = fixture.registry.subscribe();

    // A branch summary draws the busy border without a run: Pi's record is
    // idle. The blocked record after it orders the busy border before the
    // awaited edge, so any working edge would show up first.
    command(&fixture, b's');
    command(&fixture, b'b');
    fixture.wait_activity(&created.id, BLOCKED).await;
    assert_eq!(
        agent_states(&mut events),
        1,
        "only the blocked edge: the busy border changed nothing"
    );
    wait_screen(&fixture, &created.id, "Working", true).await;
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}

#[tokio::test]
async fn a_pi_screen_no_rule_matches_leaves_the_record_in_charge() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;
    let mut events = fixture.registry.subscribe();

    // A dialog drawn with ctx.ui.custom() matches no screen rule; the record
    // says working and nothing else decides.
    command(&fixture, b'x');
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    assert_eq!(
        agent_states(&mut events),
        1,
        "only the done edge: the unmatched screen changed nothing"
    );
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}

#[tokio::test]
async fn a_held_pi_record_outlives_screen_changes_until_pi_clears_it() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;
    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;

    // The idle screen cannot undo the held working record.
    command(&fixture, b'r');
    wait_screen(&fixture, &created.id, "Working", false).await;
    assert_eq!(fixture.activity(&created.id).await, Some(WORKING));

    command(&fixture, b'l');
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::Screen))
        .await;
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
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
    fixture.end().await;
}

/// The probe reply an attached terminal would send for the probe it saw.
const CLIENT_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

#[tokio::test]
async fn an_attached_clients_probe_reply_never_reaches_pi() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-reply-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;
    assert_eq!(
        read_trimmed(&fixture.scratch_file("reply.hex")).as_deref(),
        Some(REPLY_HEX),
        "the worker answered the probe"
    );

    let attach = fixture
        .registry
        .attach(&SessionAttachParams {
            session_id: created.id.clone(),
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .await
        .expect("open an attach stream");
    let redeemed = fixture
        .registry
        .redeem_attach(&attach.stream_id)
        .await
        .expect("redeem the attach stream");
    let RedeemedRuntime::Worker(mut data) = redeemed.runtime;

    // The client's terminal answers the probe it saw in the replay and types
    // the turn command in the same frame. The command runs only when the
    // reply was dropped: a leaked reply would sit in the stand-in's pending
    // line and the command would no longer match.
    let mut bytes = CLIENT_REPLY.to_vec();
    bytes.extend_from_slice(b"t\r");
    let frame = DataFrame::new(
        FrameHeader {
            version: data.version,
            stream_id: data.stream_id.clone(),
            worker_instance_id: data.worker_instance_id.clone(),
            kind: FrameKind::Input {
                write_id: WriteId::new(format!("attach-{}-1", attach.stream_id)).expect("write id"),
            },
        },
        bytes,
    )
    .expect("attach input frame");
    write_frame(&mut data.stream, &frame)
        .await
        .expect("send the client's input");

    fixture.wait_activity(&created.id, WORKING).await;
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}

#[tokio::test]
async fn a_record_is_ignored_under_a_manifest_without_the_table() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-nested-fifo").await;
    wait_screen(&fixture, &created.id, "say hi", false).await;

    // The stand-in is the shell session's own program, so the shell manifest
    // stays active. Its `working` record is ignored because that manifest has
    // no `[program_status]` table. The next screen change arrives after the
    // record in the byte stream, so the record has been seen once it shows.
    command(&fixture, b'g');
    wait_screen(&fixture, &created.id, "say hi", true).await;
    command(&fixture, b'r');
    wait_screen(&fixture, &created.id, "say hi", false).await;
    let info = fixture
        .registry
        .inspect(&created.id)
        .await
        .expect("inspect session");
    assert_ne!(info.state_source, StateSource::OscProgress, "{info:?}");
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}

#[tokio::test]
async fn a_nested_pis_held_record_does_not_outlive_the_pi() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::new().await;
    let created = fixture.start("fake-shell-es4").await;
    wait_until("procwatch to attribute the nested Pi", || async {
        let info = fixture.registry.inspect(&created.id).await.ok()?;
        (info.active_agent.as_deref() == Some("pi")).then_some(())
    })
    .await;

    // Under the Pi manifest the nested Pi's record is held and read.
    std::fs::write(fixture.scratch_file("go1"), b"\n").expect("release the nested Pi");
    fixture.wait_activity(&created.id, WORKING).await;

    // The Pi exits; the exit watch clears the active agent, the detector goes
    // back to the shell manifest and the held record is gone with it.
    std::fs::write(fixture.scratch_file("go2"), b"\n").expect("let the nested Pi exit");
    wait_screen(&fixture, &created.id, "ready", true).await;
    wait_until("the shell manifest to read idle", || async {
        let info = fixture.registry.inspect(&created.id).await.ok()?;
        (info.active_agent.is_none()
            && info.state_source != StateSource::OscProgress
            && info.activity == Some(AgentActivity::Idle))
        .then_some(info)
    })
    .await;
    let _ = fixture.registry.stop(&created.id).await;
    fixture.end().await;
}
