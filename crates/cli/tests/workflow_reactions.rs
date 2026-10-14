//! End-to-end contracts of workflow event reactions: the dispatcher unit that
//! the service manager runs for the enabled plugins, the event it delivers,
//! and the drop accounting of a plugin whose reactions cannot keep up.
//!
//! Ignored unless `POHUNEK_SYSTEMD_E2E=1` opts in, like the other real-systemd
//! suites. Each scenario installs a fresh installation through
//! `support/workflow_units.rs`, which records the service installation and
//! enables the plugin through the code `pohunek workflow enable` runs. The
//! dispatcher unit then runs `pohunek workflow __dispatch` under the real user
//! manager, and a real `pohunekd` in the same hermetic runtime root provides
//! the event stream.

// Rust guideline compliant 2026-10-13

#![cfg(target_os = "linux")]

#[path = "support/workflow_units.rs"]
mod workflow_units;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use pohunek_test_support::workers::WorkerGuard;
use pohunek_test_support::{bin_exe, worker_binary};
use serde_json::Value;

use crate::workflow_units::{native, Managed, Plugin, KNOBS};
use pohunek_cli::service::definition::identity_config;
use pohunek_cli::service::VERSION;
use pohunek_platform::supervisor::{PluginUnitKey, PluginUnitKind};

/// Plugin id of every scenario.
const PLUGIN: &str = "acme.react";

/// Agent profile the scenarios create sessions with.
const AGENT: &str = "reactions-shell";

/// Context file names of the fixture reaction, as its program writes them.
const CONTEXT_PREFIX: &str = "context.";

/// Origin files of the fixture reaction.
const ORIGIN_PREFIX: &str = "origin.";

/// Another version than this CLI, recorded for the installation.
const OTHER_VERSION: &str = "0.0.1";

/// A second plugin with reactions, for the scenarios that need several.
const OTHER_PLUGIN: &str = "acme.watch";

/// A third plugin with reactions, for the scenarios that need several.
const THIRD_PLUGIN: &str = "acme.audit";

/// Marker file that makes the fixture reaction sleep per run.
const SLOW_MARKER: &str = "slow";

/// Context cap that admits the context without its event and refuses the
/// event itself: the session event is far larger than the bare context.
const MAX_SMALL_CONTEXT_BYTES: u64 = 700;

/// Marker file that makes the fixture reaction ignore `SIGTERM` for a minute.
const WEDGE_MARKER: &str = "wedge";

/// Marker file the wedged reaction writes once it has started.
const WEDGED_PREFIX: &str = "wedged.";

/// Longest a session command or a daemon restart may take behind a wedged
/// reaction: the dispatcher never waits for reactions, so this only has to
/// cover the commands' own work, with generous headroom on a loaded host.
const WEDGE_TOLERANCE: Duration = Duration::from_secs(20);

/// How long the reaction must stay idle before a scenario counts its runs.
const SETTLE: Duration = Duration::from_secs(2);

/// The `[reactions]` knobs of the drop scenario: one queued event per plugin.
const ONE_QUEUED_EVENT: &str = "[reactions]\nqueue_depth = 1\n";

/// `pohunekd` running in the scenario's hermetic environment, stopped on drop.
struct Daemon {
    // Declared before `child`, so leftover workers are reaped first.
    _workers: WorkerGuard,
    child: Child,
}

impl Daemon {
    fn start(env: &TestEnv) -> Self {
        let workers = WorkerGuard::watch(env.root().to_path_buf());
        let log = fs::File::create(env.root().join("daemon.out")).expect("create the daemon log");
        let child = env
            .command(bin_exe("pohunek").with_file_name("pohunekd"))
            .env("SHELL", "/bin/sh")
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("duplicate the log handle"))
            .stderr(log)
            .spawn()
            .expect("spawn pohunekd");
        let socket = env.runtime_dir().join("pohunek/daemon.sock");
        poll_until("the daemon control socket accepts connections", || {
            UnixStream::connect(&socket).ok().map(drop)
        });
        Self {
            _workers: workers,
            child,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

/// The plugin's state directory in `env`.
fn state_dir(env: &TestEnv) -> PathBuf {
    env.state_home()
        .join("pohunek/workflow-plugin-state")
        .join(PLUGIN)
}

/// Declares the agent profile and creates one session with it, returning the
/// session id.
fn create_session(env: &TestEnv) -> String {
    let agents = env.config_home().join("pohunek/agents");
    fs::create_dir_all(&agents).expect("create the agent profile directory");
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o700)).expect("chmod agents");
    let profile = agents.join(format!("{AGENT}.toml"));
    fs::write(
        &profile,
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"exec /bin/cat\"]\n",
    )
    .expect("write the agent profile");
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).expect("chmod profile");
    let cwd = env.cwd().to_str().expect("UTF-8 cwd").to_owned();
    let created = env
        .command(bin_exe("pohunek"))
        .args(["session", "new", "--agent", AGENT, "--cwd", &cwd, "--json"])
        .output()
        .expect("create the session");
    assert_eq!(created.status.code(), Some(0), "{}", describe(&created));
    let document: Value = serde_json::from_slice(&created.stdout).expect("session JSON");
    document["ok"]["session"]["id"]
        .as_str()
        .or_else(|| document["ok"]["id"].as_str())
        .expect("session id")
        .to_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every file of the state directory whose name starts with `prefix`, sorted.
fn files_with(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.expect("read a state entry").path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// The contexts the reaction recorded, parsed.
fn contexts(dir: &Path) -> Vec<Value> {
    files_with(dir, CONTEXT_PREFIX)
        .iter()
        .map(|path| {
            let text = fs::read_to_string(path).expect("read a recorded context");
            serde_json::from_str(&text).expect("a recorded context is JSON")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn session_created_reaches_the_reaction_with_no_origin_pair() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new(PLUGIN).reaction("on_created", &["session_created"], "react");
    managed.enable(&plugin).await;
    let env = &managed.units.env;
    let _daemon = Daemon::start(env);

    // Sessions created before the dispatcher subscribes go unnoticed, so keep
    // creating them until one is reacted to.
    let dir = state_dir(env);
    let mut sessions = Vec::new();
    let context = poll_until("the reaction to record its context", || {
        sessions.push(create_session(env));
        contexts(&dir).into_iter().find(|context| {
            context["event"]["event"] == "session_created"
                && sessions
                    .iter()
                    .any(|session| context["event"]["payload"]["session"]["id"] == session.as_str())
        })
    });
    assert_eq!(context["invocation"]["kind"], "reaction", "{context}");
    assert_eq!(context["invocation"]["name"], "on_created", "{context}");
    assert_eq!(context["plugin"]["id"], PLUGIN, "{context}");
    assert!(context.get("event_omitted").is_none(), "{context}");
    assert!(context.get("output").is_none(), "{context}");

    // The reaction's environment never carries the session's origin pair.
    let origin = poll_until("the reaction to record its origin", || {
        files_with(&dir, ORIGIN_PREFIX)
            .first()
            .and_then(|path| fs::read_to_string(path).ok())
    });
    assert_eq!(origin, "absent");
}

/// While an enabled plugin's reactions keep the dispatcher unit installed,
/// `service uninstall` is refused with that plugin's disable command, the
/// step that removes the dispatcher.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn service_uninstall_names_the_enabled_plugin_that_reacts() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new(PLUGIN).reaction("on_created", &["session_created"], "react");
    managed.enable(&plugin).await;

    let output = managed
        .units
        .command(&["service", "uninstall", "--json"])
        .output()
        .expect("run pohunek service uninstall");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let document: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("a JSON envelope: {}", describe(&output)));
    assert_eq!(
        document["err"]["code"], "workflow_units_installed",
        "{document}"
    );
    let message = document["err"]["msg"].as_str().expect("a message");
    assert!(
        message.contains(&format!("run `pohunek workflow disable {PLUGIN}` first")),
        "{document}"
    );
    assert!(!message.contains("dispatcher"), "{document}");
}

/// Under a service installation of another pohunek version, disabling a
/// plugin with reactions succeeds and stops and removes the dispatcher unit,
/// also while other enabled plugins react, and later disables succeed with no
/// dispatcher left to remove: removing a unit runs no trampoline, as for a
/// plugin's own units, and only this version may write the unit again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn disabling_removes_the_dispatcher_under_an_installation_of_another_version() {
    assert_ne!(VERSION, OTHER_VERSION);
    let mut managed = Managed::new(KNOBS);
    for id in [PLUGIN, OTHER_PLUGIN, THIRD_PLUGIN] {
        let plugin = Plugin::new(id).reaction("on_created", &["session_created"], "react");
        managed.enable(&plugin).await;
    }
    let record = managed
        .units
        .env
        .state_home()
        .join("pohunek/workflow-plugins/units/REACTIONS.json");
    assert!(record.exists(), "enabling recorded the dispatcher unit");
    assert_eq!(
        managed.loaded().len(),
        1,
        "only the dispatcher unit is loaded"
    );
    identity_config(
        &managed.units.context,
        &managed.units.env.root().join("prefix"),
        OTHER_VERSION,
    )
    .expect("the installation configuration")
    .write(&managed.units.context.config_path())
    .expect("record the other version");

    // In process, like `enable`: the CLI's own child has no bus to the real
    // user manager in the hermetic environment.
    managed.disable(PLUGIN).await;
    assert!(!record.exists(), "the dispatcher unit is still recorded");
    assert!(managed.loaded().is_empty(), "{:?}", managed.loaded());
    // A plugin still reacts, and no dispatcher is left to remove.
    managed.disable(THIRD_PLUGIN).await;
    managed.disable(OTHER_PLUGIN).await;
    assert!(!record.exists(), "the dispatcher unit was recorded again");
    assert!(managed.loaded().is_empty(), "{:?}", managed.loaded());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_full_queue_drops_events_and_keeps_delivering_later_ones() {
    let mut managed = Managed::new(&format!("{KNOBS}\n{ONE_QUEUED_EVENT}"));
    let plugin = Plugin::new(PLUGIN).reaction("on_created", &["session_created"], "react");
    managed.enable(&plugin).await;
    let env = &managed.units.env;
    let dir = state_dir(env);
    let _daemon = Daemon::start(env);

    // Sessions created before the dispatcher subscribes go unnoticed. Once one
    // reaction has run, the stream is connected and the reaction is idle.
    poll_until("the dispatcher to react to a session", || {
        create_session(env);
        (!contexts(&dir).is_empty()).then_some(())
    });
    // Probe sessions may still be queued behind the first run, so wait until
    // the reaction has been idle for a while before counting from here.
    let before = settled_contexts(&dir);
    fs::write(dir.join(SLOW_MARKER), b"").expect("mark the reaction slow");

    for _ in 0..10 {
        create_session(env);
    }
    // Drops are summed over every record, since a probe session can already
    // have been shed before the burst started.
    let dropped = poll_until("drop records counting the shed events", || {
        let total: u64 = run_records(&dir)
            .iter()
            .filter(|record| record["outcome"] == "dropped")
            .filter_map(|record| record["dropped"].as_u64())
            .sum();
        (total >= 8).then_some(total)
    });
    assert!(dropped >= 8, "dropped count {dropped}");

    // One run was in progress and one event waited in the queue, so exactly
    // two runs happen for the ten events.
    poll_until("the two queued runs", || {
        (contexts(&dir).len() == before + 2).then_some(())
    });

    // Events arriving after the overflow are still delivered.
    fs::remove_file(dir.join(SLOW_MARKER)).expect("clear the slow marker");
    let later = create_session(env);
    poll_until("a run for a session created after the overflow", || {
        contexts(&dir)
            .into_iter()
            .any(|context| context["event"]["payload"]["session"]["id"] == later.as_str())
            .then_some(())
    });
}

/// The number of contexts once the count has stayed unchanged for a while.
fn settled_contexts(dir: &Path) -> usize {
    let mut last = (0, Instant::now());
    poll_until("the reaction to settle", || {
        let now = contexts(dir).len();
        if now != last.0 {
            last = (now, Instant::now());
        }
        (now > 0 && last.1.elapsed() >= SETTLE).then_some(now)
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn an_oversized_event_is_omitted_and_its_session_named() {
    let knobs = KNOBS.replacen(
        "kill_grace_ms = 300\n",
        &format!("kill_grace_ms = 300\nmax_context_bytes = {MAX_SMALL_CONTEXT_BYTES}\n"),
        1,
    );
    let mut managed = Managed::new(&knobs);
    let plugin = Plugin::new(PLUGIN).reaction("on_created", &["session_created"], "react");
    managed.enable(&plugin).await;
    let env = &managed.units.env;
    let dir = state_dir(env);
    let _daemon = Daemon::start(env);

    let mut sessions = Vec::new();
    let context = poll_until("the reaction to record an omitted event", || {
        sessions.push(create_session(env));
        contexts(&dir).into_iter().find(|context| {
            context["event_omitted"] == "too_large"
                && sessions
                    .iter()
                    .any(|session| context["session_id"] == session.as_str())
        })
    });
    assert!(context.get("event").is_none(), "{context}");
    assert_eq!(context["invocation"]["kind"], "reaction", "{context}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_wedged_reaction_delays_neither_session_commands_nor_a_daemon_restart() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new(PLUGIN).reaction("on_created", &["session_created"], "react");
    managed.enable(&plugin).await;
    let env = &managed.units.env;
    let dir = state_dir(env);
    let mut daemon = Daemon::start(env);

    // The reaction ignores SIGTERM and keeps its run for a minute.
    fs::write(dir.join(WEDGE_MARKER), b"").expect("mark the reaction wedged");
    let mut sessions = Vec::new();
    poll_until("the wedged reaction to start", || {
        sessions.push(create_session(env));
        files_with(&dir, WEDGED_PREFIX).first().map(drop)
    });

    let started = Instant::now();
    let session = create_session(env);
    let stopped = env
        .command(bin_exe("pohunek"))
        .args(["session", "stop", &session, "--json"])
        .output()
        .expect("stop the session");
    assert_eq!(stopped.status.code(), Some(0), "{}", describe(&stopped));
    assert!(
        started.elapsed() < WEDGE_TOLERANCE,
        "session commands took {:?} behind a wedged reaction",
        started.elapsed()
    );

    let restarted = Instant::now();
    drop(daemon);
    daemon = Daemon::start(env);
    assert!(
        restarted.elapsed() < WEDGE_TOLERANCE,
        "a daemon restart took {:?} behind a wedged reaction",
        restarted.elapsed()
    );
    drop(daemon);
}

/// Backoff of the retry scenario: the first delay and the cap, in milliseconds.
const RETRY_INITIAL_MS: u64 = 100;
const RETRY_MAX_MS: u64 = 400;

/// Kills a child dispatcher on drop.
struct Dispatcher(Child);

impl Drop for Dispatcher {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

/// The delays the dispatcher logged in its retry messages, in order.
fn retry_delays(log: &Path) -> Vec<u64> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let rest = line.split("retrying in ").nth(1)?;
            rest.split(" ms").next()?.parse().ok()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_dispatcher_without_a_daemon_retries_with_backoff_then_delivers() {
    let knobs = format!(
        "{KNOBS}\n[reactions]\nreconnect_initial_ms = {RETRY_INITIAL_MS}\nreconnect_max_ms = {RETRY_MAX_MS}\n"
    );
    let mut managed = Managed::new(&knobs);
    let plugin = Plugin::new(PLUGIN).reaction("on_created", &["session_created"], "react");
    managed.enable(&plugin).await;
    let env = &managed.units.env;
    let dir = state_dir(env);

    // The scenario runs its own dispatcher as a child process, so the unit that
    // enabling installed is stopped and removed first.
    let namespace = managed.units.context.namespace().expect("the namespace");
    let key = PluginUnitKey::new("pohunek:reactions", PluginUnitKind::Service, "dispatch")
        .expect("the dispatcher key");
    native::remove(&namespace, &[key], managed.units.context.supervisor_dir());

    let log = env.root().join("dispatcher.err");
    let stderr = fs::File::create(&log).expect("create the dispatcher log");
    let child = env
        .command(bin_exe("pohunek"))
        .args(["workflow", "__dispatch"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn the dispatcher");
    let _dispatcher = Dispatcher(child);

    // Without a daemon the dispatcher never creates a socket and retries with
    // doubling delays that stop at the cap.
    let socket = env.runtime_dir().join("pohunek/daemon.sock");
    let delays = poll_until("five retries without a daemon", || {
        assert!(!socket.exists(), "the dispatcher must not start the daemon");
        let delays = retry_delays(&log);
        (delays.len() >= 5).then_some(delays)
    });
    assert_eq!(delays[0], RETRY_INITIAL_MS, "{delays:?}");
    for pair in delays.windows(2) {
        assert_eq!(pair[1], (pair[0] * 2).min(RETRY_MAX_MS), "{delays:?}");
    }
    assert!(delays.contains(&RETRY_MAX_MS), "{delays:?}");

    // Once the daemon is up, the dispatcher connects and reacts to new sessions.
    let _daemon = Daemon::start(env);
    let mut sessions = Vec::new();
    poll_until("the reaction to run after the daemon started", || {
        sessions.push(create_session(env));
        contexts(&dir).into_iter().find(|context| {
            context["event"]["event"] == "session_created"
                && sessions
                    .iter()
                    .any(|session| context["event"]["payload"]["session"]["id"] == session.as_str())
        })
    });
}

/// Run records of the plugin's active run log, parsed.
fn run_records(dir: &Path) -> Vec<Value> {
    let text = fs::read_to_string(dir.join("logs/runs.jsonl")).unwrap_or_default();
    text.lines()
        .map(|line| serde_json::from_str(line).expect("a run record is one JSON line"))
        .collect()
}
