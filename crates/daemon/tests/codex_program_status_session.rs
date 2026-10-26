//! The official Codex package reading OSC 7501 program status through a real
//! session: worker, PTY, registry and the installed package.
//!
//! The program is a POSIX shell stand-in for Codex 0.163 and later: it sets an
//! idle terminal title, draws a composer and reports its state by raw root
//! records (`app=codex`) when it reads a command. Codex never waits for a
//! probe reply, so the sessions here also prove that the worker writes none.

// Rust guideline compliant 2026-10-23

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
use pohunek_test_support::worker_binary;
use protocol::{
    AgentActivity, SessionId, SessionInfo, SessionInputParams, SessionInputWait, SessionNewParams,
    StateSource, ENV_DAEMON_ID, ENV_SESSION_ID,
};

const TEST_COLS: u16 = 100;
const TEST_ROWS: u16 = 24;
/// The `app` Codex's records carry.
const APP: &str = "codex";
/// The first two bytes of a probe reply, as hex: `ESC ]`. Typed input starts
/// with `ESC [` (bracketed paste).
const REPLY_PREFIX_HEX: &str = "1b5d";
/// The release before the first Codex that reports program status, which the
/// package still supports and reads from the title.
const PREVIOUS_BANNER: &str = "codex-cli 0.160.0";
/// Bound of a wait that must be satisfied by the stand-in agent.
const WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;
/// Poll interval of the process watcher in the nested-agent scenario.
const PROCWATCH_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// Seconds the managed shell stays alive after its agent exits.
const SHELL_LINGER_SECS: u64 = 600;

/// The stand-in agent. `$1` is the mode (`records` or `silent`), `$2` the
/// scratch directory, `$3` where commands come from (`stdin` or `fifo`).
///
/// It answers the version probe with `banner`, prints a forged probe the way a
/// hostile program could, sets an idle title and draws a composer. A command
/// read from the terminal (bracketed paste markers dropped) runs on Enter, as a
/// submitted prompt does; one read
/// from the `cmd` FIFO runs at once. The commands: `g` reports working, `d`
/// done, `b` blocked, `s` working and done in one write, `t` is a turn that
/// reports working until the FIFO yields a byte, then done.
fn agent_script(banner: &str) -> String {
    format!(
        r#"#!/bin/sh
if [ "$1" = --version ]; then printf '%s\n' '{banner}'; exit 0; fi
mode=$1; dir=$2; source=$3
stty raw -echo
printf '\033]7501;?\033\\'
printf '\033]2;work\007'
printf '\033[2J\033[H'
printf '  >_ OpenAI Codex\r\n\r\n'
printf '› Ask Codex to do anything\r\n\r\n'
printf '  stub-model default\r\n'
printf '  ? for shortcuts'
emit() {{ if [ "$mode" = records ]; then printf '\033]7501;state=%s:app={APP}\033\\' "$1"; fi; }}
emit idle
if [ "$mode" = typed ]; then
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
    b) printf '\033]7501;state=blocked:app={APP}:kind=permission\033\\' ;;
    s) if [ "$mode" = records ]; then printf '\033]7501;state=working:app={APP}\033\\\033]7501;state=done:app={APP}\033\\'; fi ;;
    t) emit working; head -c 1 <&3 > /dev/null; emit done ;;
    q) exit 0 ;;
  esac
}}
while :; do
  if [ "$source" = fifo ]; then
    run "$(head -c 1 <&3)"
  else
    c=$(head -c 1)
    if [ "$c" = "$cr" ]; then
      run "$(printf '%s' "$pending" | tr -d '\033' | sed 's/\[20[01]~//g' | cut -c1)"
      pending=
    else pending="$pending$c"; fi
  fi
done
"#
    )
}

/// The nested stand-in: started by a shell session, it reports working until
/// the `stop` file appears, then exits. A record the daemon receives before it
/// has switched to Codex's manifest is dropped with the switch, so the agent
/// repeats it.
fn nested_agent_script(dir: &Path) -> String {
    format!(
        r"#!/bin/sh
printf '\033]2;work\007'
while [ ! -e '{}' ]; do
  printf '\033]7501;state=working:app={APP}\033\\'
  sleep 0.3
done
",
        dir.join("stop").display()
    )
}

/// One worker-backed registry with the stand-in agent installed as profiles
/// over the official Codex package.
struct Fixture {
    env: TestEnv,
    registry: SessionRegistry,
    scratch: PathBuf,
}

impl Fixture {
    async fn new(shell_command: ShellCommand) -> Self {
        let env = TestEnv::new().expect("create the test environment");
        let scratch = env.cwd().join("scratch");
        std::fs::create_dir(&scratch).expect("create the scratch directory");
        let agents = env.root().join("agents");
        std::fs::create_dir_all(&agents).expect("create the agents directory");
        for (name, banner, mode, source) in [
            (
                "fake-codex-stdin",
                official_runtimes::supported_banner("codex"),
                "records",
                "stdin",
            ),
            (
                "fake-codex-fifo",
                official_runtimes::supported_banner("codex"),
                "records",
                "fifo",
            ),
            (
                "fake-codex-typed",
                official_runtimes::supported_banner("codex"),
                "typed",
                "stdin",
            ),
            (
                "fake-codex-previous",
                PREVIOUS_BANNER.to_owned(),
                "silent",
                "fifo",
            ),
        ] {
            let program = env.root().join(format!("{name}-program"));
            std::fs::write(&program, agent_script(&banner)).expect("write the stand-in agent");
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                .expect("make the stand-in agent executable");
            std::fs::write(
                agents.join(format!("{name}.toml")),
                format!(
                    "base = \"codex\"\n{}program = \"{}\"\nargs = [\"{mode}\", \"{}\", \"{source}\"]\n\n[env]\nPATH = \"/usr/bin:/bin\"\n",
                    official_runtimes::profile_pin("codex"),
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
                agents_dir: Some(agents),
                host_state_dir: Some(env.root().join("host-state")),
                procwatch_poll: PROCWATCH_POLL,
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

    async fn with_agent_profiles() -> Self {
        Self::new(ShellCommand::new("/bin/sh", ["-c", "sleep 600"])).await
    }

    /// Starts the session of the agent `profile`.
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

const WORKING: (AgentActivity, StateSource) = (AgentActivity::Working, StateSource::OscProgress);
const IDLE: (AgentActivity, StateSource) = (AgentActivity::Idle, StateSource::OscProgress);

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

/// ES1 (E1, E2): Codex's records drive a waited input: working, then idle.
#[tokio::test]
async fn codex_reports_turns_through_program_status() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::with_agent_profiles().await;
    let created = fixture.start("fake-codex-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;

    let working = fixture
        .input(&created.id, "t", AgentActivity::Working)
        .await
        .expect("input until working");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    assert_eq!(working.activity_source, Some(StateSource::OscProgress));
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;

    let release = async {
        fixture.wait_activity(&created.id, WORKING).await;
        command(&fixture, b'd');
    };
    let idle = fixture.input(&created.id, "t", AgentActivity::Idle);
    let (idle, ()) = tokio::join!(idle, release);
    let idle = idle.expect("input until idle");
    assert_eq!(idle.activity, Some(AgentActivity::Idle));
    assert_eq!(idle.activity_source, Some(StateSource::OscProgress));
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// L1p: a turn whose working and done records arrive in one chunk is still a
/// working edge for the waited input.
#[tokio::test]
async fn a_working_record_followed_by_done_in_one_chunk_satisfies_a_working_wait() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::with_agent_profiles().await;
    let created = fixture.start("fake-codex-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;

    let working = fixture
        .input(&created.id, "s", AgentActivity::Working)
        .await
        .expect("input until working");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    fixture.wait_activity(&created.id, IDLE).await;
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// A record with no input changes the activity.
#[tokio::test]
async fn a_codex_record_without_input_changes_the_activity() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::with_agent_profiles().await;
    let created = fixture.start("fake-codex-fifo").await;
    fixture.wait_activity(&created.id, IDLE).await;

    command(&fixture, b'g');
    fixture.wait_activity(&created.id, WORKING).await;
    command(&fixture, b'g');
    command(&fixture, b'g');
    command(&fixture, b'd');
    fixture.wait_activity(&created.id, IDLE).await;
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// E3: a blocked record is blocked activity and refuses a waited input.
#[tokio::test]
async fn a_codex_blocked_record_refuses_a_waited_input() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::with_agent_profiles().await;
    let created = fixture.start("fake-codex-stdin").await;
    fixture.wait_activity(&created.id, IDLE).await;

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

/// W2, W8, S5: a Codex session never gets a probe reply, whatever the program
/// prints, so nothing but the typed key reaches its input.
#[tokio::test]
async fn a_codex_session_never_gets_a_probe_reply() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::with_agent_profiles().await;
    let created = fixture.start("fake-codex-typed").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscTitle))
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
    assert_eq!(
        typed.len(),
        REPLY_PREFIX_HEX.len(),
        "two input bytes: {typed}"
    );
    assert_ne!(typed, REPLY_PREFIX_HEX, "the worker writes no reply");
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// N1: an agent of the previous supported release (banner 0.160.0) launches
/// and reads from the title.
#[tokio::test]
async fn the_previous_supported_codex_release_launches_and_reads_from_the_title() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let fixture = Fixture::with_agent_profiles().await;
    let created = fixture.start("fake-codex-previous").await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscTitle))
        .await;
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// ES4 (M2, S3): a Codex started below a shell session reports through its
/// record; once it exits, the shell reads its own title again.
#[tokio::test]
async fn a_held_working_record_ends_with_the_nested_codex() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let env_probe = TestEnv::new().expect("create a scratch environment for the paths");
    let nested_dir = env_probe.cwd().join("nested");
    std::fs::create_dir_all(&nested_dir).expect("create the nested agent directory");
    let nested = nested_dir.join("codex");
    let stop = nested_dir.join("stop");
    std::fs::write(&nested, nested_agent_script(&nested_dir)).expect("write the nested agent");
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755))
        .expect("make the nested agent executable");
    let script = format!(
        "{} & wait $!; printf '\\033]2;ready\\007'; sleep {SHELL_LINGER_SECS}",
        nested.display()
    );
    let fixture = Fixture::new(ShellCommand::new("/bin/sh", ["-c", script.as_str()])).await;
    let created = {
        let mut process_env = ProcessEnv::lock();
        process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
        fixture
            .registry
            .create(SessionNewParams {
                name: None,
                agent: "shell".to_owned(),
                cwd: Some(fixture.env.cwd().to_path_buf()),
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
            .expect("create the shell session")
    };

    wait_until("the nested Codex to be observed", || async {
        let info = fixture
            .registry
            .inspect(&created.id)
            .await
            .expect("inspect session");
        (info.active_agent.as_deref() == Some("codex")).then_some(())
    })
    .await;
    fixture.wait_activity(&created.id, WORKING).await;

    std::fs::write(&stop, b"").expect("release the nested agent");
    wait_until("the nested Codex to be cleared", || async {
        let info = fixture
            .registry
            .inspect(&created.id)
            .await
            .expect("inspect session");
        (info.active_agent.is_none()).then_some(())
    })
    .await;
    fixture
        .wait_activity(&created.id, (AgentActivity::Idle, StateSource::OscTitle))
        .await;
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
    drop(env_probe);
}
