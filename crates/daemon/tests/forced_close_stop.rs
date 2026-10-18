//! A stop whose PTY output is force-closed ends the session with its root's
//! exit only once the runtime's escaped processes are proven gone.
//!
//! A descendant outside the root's process group (a `setsid` child) keeps the
//! PTY slave open after the root dies, so the worker has to force the output
//! closed. The root's exit is known and reaped. Before the worker commits that
//! exit, it sweeps every process carrying the runtime's marker: a holder it can
//! attribute and terminate lets the session end `stopped` with the exit and
//! the forced close visible, and that outcome survives a daemon restart. A
//! holder whose environment the kernel withholds can be neither attributed nor
//! dismissed, so the worker faults the runtime instead, the session is never
//! terminal, and the holder is never signalled. A durable stop intent decides
//! the outcome of a root that exited on its own just before the stop, both when
//! the worker's commit lands after the stop gave up waiting and when a
//! restarted daemon imports the exited runtime from a worker still answering.

// Rust guideline compliant 2026-10-13

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pohunek_daemon::procwatch::{HostInspector, ProcessIdentity, ProcessInspector};
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::host_lock::{HostLock, UNREADABLE_HOLDERS};
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{guard, poll_until, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{
    RuntimeState, SessionId, SessionInfo, SessionNewParams, SessionState, ENV_DAEMON_ID,
    ENV_SESSION_ID,
};

/// Seconds the root sleeps if the stop never kills it.
const ROOT_SLEEP_SECONDS: u32 = 30;

/// Libtest name of the escaped holder this binary runs as a fixture process.
const HOLDER_TEST: &str = "escaped_holder";

/// Filter word that marks a launch of this binary as the escaped holder.
///
/// Under `--exact` it matches no test, so a plain `--ignored` run of the binary
/// selects only [`escaped_holder`], which then returns at once.
const HOLDER_MARKER: &str = "escaped-holder-marker";

/// Filter word that makes the holder hide its environment from same-user
/// readers, as a non-dumpable helper does.
const NON_DUMPABLE: &str = "escaped-holder-non-dumpable";

/// Upper bound on how long a holder runs when its barrier is never removed.
///
/// Longer than the hang guard of any scenario here, so only a crashed run that
/// could not remove the barrier ever reaches it.
const HOLDER_LIFETIME: std::time::Duration = std::time::Duration::from_secs(300);

/// Pause between the holder's checks of its barrier.
const HOLDER_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// The daemon's supervision sweep grace in these scenarios. The lost-runtime
/// sweep waits one grace for unreadable markers to settle before it reports
/// the cleanup unconfirmed; the subprocess default is 5 s.
const SWEEP_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

/// Text shared by the worker's sweep events of a force-closed runtime.
const SWEEP_EVENTS: &str = "of a force-closed runtime";

/// Text of the worker event that names a process keeping the cleanup open.
const BLOCKER_EVENT: &str = "a process kept the cleanup of a force-closed runtime unconfirmed";

/// File name of the scenario's daemon store, below its state home.
const STORE_FILE: &str = "metadata.jsonl";

/// Loss reason of a lost runtime whose marked processes the daemon could not
/// prove gone.
const RUNTIME_LOST_CLEANUP_UNCONFIRMED: &str = "runtime_lost_cleanup_unconfirmed";

/// The escaped PTY holder: run by the scenarios, never on its own.
///
/// Started with [`HOLDER_MARKER`], the barrier directory and the ready file as
/// its last arguments, it optionally turns itself non-dumpable, publishes its
/// PID in the ready file and runs while the barrier exists. It holds the PTY
/// slave through a descriptor the launching script passes.
#[test]
#[ignore = "fixture process image of the forced-close scenarios"]
fn escaped_holder() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|argument| argument == HOLDER_MARKER) {
        return;
    }
    let [.., barrier, ready] = args.as_slice() else {
        panic!("the holder needs its barrier and ready paths");
    };
    if args.iter().any(|argument| argument == NON_DUMPABLE) {
        nix::sys::prctl::set_dumpable(false).expect("clear the dumpable flag");
    }
    std::fs::write(ready, std::process::id().to_string()).expect("publish the holder PID");
    let started = std::time::Instant::now();
    while Path::new(barrier).is_dir() && started.elapsed() < HOLDER_LIFETIME {
        // timing-allowed: #402 the holder's own liveness poll; no scenario waits on this sleep, the barrier ends it
        std::thread::sleep(HOLDER_POLL);
    }
}

/// Removes the holder's barrier when dropped, also while a failure unwinds,
/// so the holder ends.
struct ReleaseHolder(PathBuf);

impl Drop for ReleaseHolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// The escaped process that keeps the session's PTY open.
#[derive(Debug, Clone, Copy)]
enum Holder {
    /// This test binary, ignoring hangup and termination.
    Image,
    /// This test binary, ignoring hangup and termination and hiding its
    /// environment from same-user readers.
    NonDumpable,
    /// [`FORKING_HOLDER`] with this many generations after the first: each
    /// generation answers `SIGTERM` by forking the next and exiting.
    Forking(u8),
}

/// Shell that runs [`FORKING_HOLDER`]; its `read -t` waits without starting a
/// process, so the holder's only processes are its generations.
const BASH: &str = "/bin/bash";

/// Escaped holder whose every generation answers `SIGTERM` by forking the next
/// one and exiting, and whose last generation just exits.
///
/// Arguments: generations after the first, the barrier directory, the
/// directory where each generation publishes its PID as `gen-<n>`, and a FIFO
/// that nobody writes, read with a timeout so the wait starts no process. A
/// generation is a forked subshell, never a new image, so its markers stay
/// readable from its first instruction on, and it exists before its parent
/// exits: a sweep pass that ends one generation leaves exactly the next.
const FORKING_HOLDER: &str = r#"generation() {
  local depth=$1
  trap 'if [ "$depth" -gt 0 ]; then generation "$((depth - 1))" & fi; exit 0' TERM
  printf '%s' "$BASHPID" > "$dir/gen-$depth"
  while [ -d "$barrier" ]; do read -r -t 0.05 -u 4 _; done
}
barrier=$2
dir=$3
exec 4<>"$4"
generation "$1"
"#;

/// What the session's root does once its escaped holder runs.
#[derive(Debug, Clone, Copy)]
enum Root {
    /// Sleeps until a stop kills it.
    Sleeps,
    /// Exits on its own with this status while the holder keeps the PTY open.
    Exits(u8),
}

/// One scenario: its environment, the holder's barrier and ready file, and
/// the registry configuration every daemon instance of the scenario shares.
struct Scenario {
    env: TestEnv,
    ready: PathBuf,
    config: SessionRegistryConfig,
    release: ReleaseHolder,
    /// Keeps other test runs' unreadable holders from overlapping this
    /// scenario's sweeps; declared last, so it is released last.
    _host_lock: HostLock,
}

impl Scenario {
    /// Prepares a shell session whose root starts an escaped holder and waits
    /// until the holder runs. The root then becomes a process that dies on
    /// `SIGTERM`, or, with [`Root::Exits`], exits on its own.
    fn new(holder: Holder, root: Root) -> Self {
        let host_lock = HostLock::acquire(
            UNREADABLE_HOLDERS,
            std::thread::current().name().unwrap_or("forced_close_stop"),
        );
        let env = TestEnv::new().expect("create the test environment");
        let barrier = env.root().join("holder-barrier");
        std::fs::create_dir(&barrier).expect("holder barrier");
        let tail = match root {
            Root::Sleeps => format!("exec sleep {ROOT_SLEEP_SECONDS}"),
            Root::Exits(code) => format!("exit {code}"),
        };
        let (ready, launch) = match holder {
            Holder::Image | Holder::NonDumpable => {
                let ready = env.root().join("holder-ready");
                let image = std::env::current_exe().expect("path of the running test binary");
                let mode = if matches!(holder, Holder::NonDumpable) {
                    NON_DUMPABLE
                } else {
                    ""
                };
                // The holder ignores hangup and termination, so only
                // `SIGKILL` ends it; fd 3 keeps the PTY slave open in it while
                // libtest's own output goes to /dev/null.
                let [exact, holder_test] =
                    pohunek_test_support::exact_test_filter(module_path!(), HOLDER_TEST);
                let launch = format!(
                    "(trap '' TERM; exec setsid '{image}' {exact} {holder_test} --ignored {HOLDER_MARKER} {mode} '{barrier}' '{ready}' 3>&1 </dev/null >/dev/null 2>&1) &",
                    image = image.display(),
                    barrier = barrier.display(),
                    ready = ready.display(),
                );
                (ready, launch)
            }
            Holder::Forking(generations) => {
                let generations_dir = env.root().join("holder-generations");
                std::fs::create_dir(&generations_dir).expect("holder generations directory");
                let fifo = env.root().join("holder-idle");
                nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU)
                    .expect("holder idle fifo");
                let chain = env.root().join("forking-holder.sh");
                std::fs::write(&chain, FORKING_HOLDER).expect("write the forking holder");
                // fd 3 keeps the PTY slave open in every generation.
                let launch = format!(
                    "(exec setsid {BASH} '{chain}' {generations} '{barrier}' '{dir}' '{fifo}' 3>&1 </dev/null >/dev/null 2>&1) &",
                    chain = chain.display(),
                    barrier = barrier.display(),
                    dir = generations_dir.display(),
                    fifo = fifo.display(),
                );
                (generations_dir.join(format!("gen-{generations}")), launch)
            }
        };
        let script = env.root().join("escape.sh");
        std::fs::write(
            &script,
            format!(
                "trap '' HUP\n\
                 {launch}\n\
                 while [ ! -s '{ready}' ]; do sleep 0.01; done\n\
                 {tail}\n",
                ready = ready.display(),
            ),
        )
        .expect("write the session script");
        let config = SessionRegistryConfig {
            shell_command: ShellCommand::new(
                "/bin/sh",
                [script.to_str().expect("UTF-8 script path")],
            ),
            store_path: Some(env.state_home().join(STORE_FILE)),
            ..SessionRegistryConfig::default()
        };
        Self {
            env,
            ready,
            config,
            release: ReleaseHolder(barrier),
            _host_lock: host_lock,
        }
    }

    /// A daemon instance over this scenario's store and worker roots, with a
    /// launcher of its own: the workers it starts end with it.
    fn registry(&self) -> SessionRegistry {
        self.registry_with(Arc::new(SubprocessWorkerLauncher::new()))
    }

    /// A daemon instance whose workers belong to `launcher`, so they outlive
    /// the daemon instance while the launcher is kept.
    fn registry_with(&self, launcher: Arc<SubprocessWorkerLauncher>) -> SessionRegistry {
        let worker_environment = SubprocessWorkerEnvironment {
            runtime_home: self.env.runtime_dir().to_path_buf(),
            state_home: self.env.state_home().to_path_buf(),
            data_home: self.env.data_home().to_path_buf(),
            config_home: self.env.config_home().to_path_buf(),
            cache_home: self.env.cache_home().to_path_buf(),
            home: self.env.home().to_path_buf(),
            daemon_socket: self.env.root().join("daemon.sock"),
        };
        let mut config = self.config.clone();
        config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
        config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
        let mut supervision = worker_environment
            .supervision(worker_binary())
            .with_environment_source(EnvironmentSource::fixed(self.env.environment().clone()));
        supervision.sweep_grace = SWEEP_GRACE;
        config.supervision = Some(supervision);
        SessionRegistry::new_with_launcher_and_inspector(
            config,
            launcher,
            Arc::new(HostInspector::new()),
        )
    }

    /// Creates the session and returns it with the identity of the running
    /// holder.
    async fn start(&self, registry: &SessionRegistry) -> (SessionInfo, ProcessIdentity) {
        let created = {
            // A test run inside a pohunek session must not stamp the worker
            // with that session's markers.
            let mut process_env = ProcessEnv::lock();
            process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
            registry
                .create(SessionNewParams {
                    name: Some("forced-close".to_owned()),
                    agent: "shell".to_owned(),
                    cwd: Some(self.env.cwd().to_path_buf()),
                    cols: 80,
                    rows: 24,
                    project: None,
                    repo: None,
                    branch: None,
                    base_branch: None,
                    input: None,
                    extended_input_ready_wait: None,
                    metadata: BTreeMap::new(),
                })
                .await
                .expect("create the session")
        };
        let holder_pid: u32 = poll_until("the escaped holder to hold the PTY", || {
            std::fs::read_to_string(&self.ready)
                .ok()?
                .trim()
                .parse()
                .ok()
        });
        let holder = HostInspector::new()
            .identity(holder_pid)
            .expect("inspect the escaped holder")
            .expect("the escaped holder runs");
        (created, holder)
    }

    /// The daemon store of this scenario.
    fn store_path(&self) -> PathBuf {
        self.env.state_home().join(STORE_FILE)
    }

    /// The single worker journal of this scenario.
    fn worker_journal(&self) -> serde_json::Value {
        let state_root = self.env.state_home().join("pohunek/workers");
        let session = std::fs::read_dir(state_root)
            .expect("worker state root")
            .next()
            .expect("one session directory")
            .expect("session directory entry")
            .path();
        let journal = std::fs::read_dir(session)
            .expect("session journal directory")
            .map(|entry| entry.expect("journal entry").path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .expect("a worker journal");
        serde_json::from_slice(&std::fs::read(journal).expect("read the journal"))
            .expect("the journal is JSON")
    }

    /// The worker's sweep events, which name every process that kept a
    /// cleanup unconfirmed; failure messages carry them.
    fn worker_sweep_log(&self) -> String {
        let logs = self.env.state_home().join("pohunek/logs");
        let Ok(entries) = std::fs::read_dir(logs) else {
            return String::new();
        };
        entries
            .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path()).ok())
            .flat_map(|log| {
                log.lines()
                    .filter(|line| line.contains(SWEEP_EVENTS))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[tokio::test]
async fn a_forced_output_close_ends_the_session_with_the_root_exit() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let scenario = Scenario::new(Holder::Image, Root::Sleeps);
    let (created, holder, exit_code) = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("first daemon runtime");
                runtime.block_on(async {
                    let registry = scenario.registry();
                    let (created, holder) = scenario.start(&registry).await;
                    let exit_code = stop_and_check(&scenario, &registry, &created.id, holder).await;
                    registry.begin_daemon_shutdown();
                    (created, holder, exit_code)
                })
            })
            .join()
            .expect("first daemon thread")
    });

    // A daemon that crashed after the worker committed the exit, but before
    // it persisted the stopped session, left a live record with its stop
    // intent. The restarted daemon takes the exit and the forced close from
    // the worker's terminal journal alone.
    rewind_to_the_crash_window(&scenario.store_path());
    let restarted = scenario.registry();
    restarted
        .reconcile_workers()
        .await
        .expect("reconcile after the restart");
    let info = restarted
        .inspect(&created.id)
        .await
        .expect("inspect the stopped session after the restart");
    assert_eq!(info.state, SessionState::Stopped);
    assert!(
        info.output_force_closed,
        "the journal carries the forced close"
    );
    assert_eq!(info.exit_code, exit_code, "the journal carries the exit");
    assert_eq!(
        info.runtime.as_ref().map(|runtime| runtime.state),
        Some(RuntimeState::Terminal)
    );
    assert!(
        !HostInspector::new()
            .is_running(holder)
            .expect("inspect the escaped holder"),
        "nothing restarts the swept holder"
    );

    // The runtime's marked processes are gone, so the removal sweep confirms.
    restarted
        .remove(&created.id)
        .await
        .expect("a swept runtime's session can be removed");
    drop(scenario.release);
}

/// Rewrites the stored session the way a daemon left it that crashed while its
/// stop waited for the worker: the stop intent is durable, while the session
/// and its runtime are still live and carry no exit.
fn rewind_to_the_crash_window(store: &Path) {
    let stored = std::fs::read_to_string(store).expect("read the daemon store");
    let mut rewound = String::new();
    for line in stored.lines() {
        let mut record: serde_json::Value = serde_json::from_str(line).expect("store line");
        if record["kind"] == "session" {
            assert_eq!(record["desired_state"], "stopped", "{record}");
            let info = record["info"].as_object_mut().expect("session info");
            info.insert("state".to_owned(), serde_json::json!("running"));
            info.remove("exit_code");
            info.remove("output_force_closed");
            info.get_mut("runtime")
                .and_then(serde_json::Value::as_object_mut)
                .expect("session runtime")
                .insert("state".to_owned(), serde_json::json!("live"));
        }
        rewound.push_str(&serde_json::to_string(&record).expect("serialize store line"));
        rewound.push('\n');
    }
    std::fs::write(store, rewound).expect("rewrite the daemon store");
}

/// Stops the session, checks the stopped outcome of a confirmed sweep, and
/// returns the recorded exit code.
async fn stop_and_check(
    scenario: &Scenario,
    registry: &SessionRegistry,
    id: &SessionId,
    holder: ProcessIdentity,
) -> Option<i32> {
    let stopped = guard(
        "the stop of a session with a forced output close",
        Box::pin(registry.stop(id)),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "a forced output close must not fail the stop: {error:?}\nworker sweep:\n{}",
            scenario.worker_sweep_log()
        )
    });
    assert!(stopped.stopped);
    assert!(
        !HostInspector::new()
            .is_running(holder)
            .expect("inspect the escaped holder"),
        "the stop returns only after the escaped holder of the runtime was swept"
    );

    let info = registry
        .inspect(id)
        .await
        .expect("inspect the stopped session");
    assert_eq!(info.state, SessionState::Stopped);
    let runtime = info.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Terminal);
    assert!(
        info.output_force_closed,
        "the forced close stays visible on the session"
    );
    assert!(
        info.warnings.is_empty(),
        "the forced close is no worktree-setup warning: {:?}",
        info.warnings
    );
    // The root was killed by the stop, so the exit is a signal death, not a
    // success the stop could have invented.
    assert_ne!(info.exit_code, Some(0));

    let journal = scenario.worker_journal();
    assert_eq!(journal["phase"], "terminal");
    assert_eq!(journal["outcome"]["output_forced_closed"], true);
    assert_eq!(journal["outcome"]["reason"], "explicit_stop");

    assert!(
        !registry
            .stop(id)
            .await
            .expect("a repeated stop of a terminal session")
            .stopped
    );
    info.exit_code
}

#[tokio::test]
async fn an_unattributable_holder_keeps_the_forced_close_from_ending_the_session() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let scenario = Scenario::new(Holder::NonDumpable, Root::Sleeps);
    let registry = scenario.registry();
    let (created, holder) = scenario.start(&registry).await;

    let error = guard(
        "the stop of a session whose escaped holder hides its markers",
        Box::pin(registry.stop(&created.id)),
    )
    .await
    .expect_err("an unconfirmed cleanup never completes the stop");
    assert_eq!(error.code, "worker_operation_failed", "{error:?}");

    let journal = scenario.worker_journal();
    assert_eq!(journal["phase"], "faulted", "{journal}");
    assert!(
        journal["outcome"].is_null(),
        "an unconfirmed cleanup records no exit: {journal}"
    );
    let lost = wait_until("the daemon to classify the faulted runtime", || async {
        let info = registry
            .inspect(&created.id)
            .await
            .expect("inspect the session");
        let runtime = info.runtime.as_ref()?;
        (runtime.state == RuntimeState::Lost).then_some(info)
    })
    .await;
    assert_eq!(
        lost.runtime
            .as_ref()
            .and_then(|runtime| runtime.loss_reason.as_deref()),
        Some(RUNTIME_LOST_CLEANUP_UNCONFIRMED),
        "{lost:?}"
    );
    assert!(
        !matches!(
            lost.state,
            SessionState::Stopped | SessionState::Done | SessionState::Failed
        ),
        "a runtime whose processes may survive never ends the session: {lost:?}"
    );
    assert!(!lost.output_force_closed);
    // The worker names the process that kept its cleanup open.
    let holder_pid = format!("\"process.pid\":{}", holder.pid);
    let blocker = poll_until("the worker to log the blocking holder", || {
        scenario
            .worker_sweep_log()
            .lines()
            .find(|line| line.contains(BLOCKER_EVENT) && line.contains(&holder_pid))
            .map(str::to_owned)
    });
    assert!(blocker.contains("MarkersUnreadable"), "{blocker}");
    assert!(
        HostInspector::new()
            .is_running(holder)
            .expect("inspect the escaped holder"),
        "a process whose markers cannot be read is never signalled"
    );

    // Once the holder is gone, the removal sweep confirms the cleanup.
    drop(scenario.release);
    poll_until("the released holder to exit", || {
        (!HostInspector::new()
            .is_running(holder)
            .expect("inspect the escaped holder"))
        .then_some(())
    });
    registry
        .remove(&created.id)
        .await
        .expect("the session is removable once its holder is gone");
}

/// How much longer than the worker's wait for its terminal commit the
/// late-exit scenario's stop grace is.
///
/// The worker answers a stop once its terminal journal is committed, or with a
/// retryable fault after `terminal_commit_wait_timeout`. With a holder that
/// ignores `SIGTERM`, the forced-close sweep takes one stop grace before its
/// `SIGKILL`, so a grace longer than that wait by this margin makes the commit
/// land after the stop gave up, with time to spare on a loaded host.
const LATE_COMMIT_MARGIN: std::time::Duration = std::time::Duration::from_secs(4);

/// Exit status of a root that exits on its own while its holder keeps the
/// PTY open; not a success, so a natural outcome would be `failed`.
const ROOT_EXIT_STATUS: u8 = 3;

/// Whether the session's root process still runs.
fn root_runs(pid: u32) -> bool {
    let inspector = HostInspector::new();
    match inspector.identity(pid).expect("inspect the root") {
        Some(identity) => inspector.is_running(identity).expect("inspect the root"),
        None => false,
    }
}

/// Records the stop intent a daemon persists before it asks the worker to
/// stop, as a daemon left it that crashed right after that write.
fn persist_stop_intent(store: &Path) {
    let stored = std::fs::read_to_string(store).expect("read the daemon store");
    let mut rewritten = String::new();
    for line in stored.lines() {
        let mut record: serde_json::Value = serde_json::from_str(line).expect("store line");
        if record["kind"] == "session" {
            assert_eq!(record["desired_state"], "running", "{record}");
            record["desired_state"] = serde_json::json!("stopped");
        }
        rewritten.push_str(&serde_json::to_string(&record).expect("serialize store line"));
        rewritten.push('\n');
    }
    std::fs::write(store, rewritten).expect("rewrite the daemon store");
}

/// A root that exits on its own just before the stop request is still a
/// stopped session when the worker's terminal commit lands only after the stop
/// gave up waiting for it: the durable stop intent decides the outcome.
///
/// It runs for about three stop graces (about 42 s). The grace must outlast
/// the worker's 10 s terminal-commit wait, which the real `pohunek-sessiond`
/// fixes and no configuration it reads can shorten.
#[tokio::test]
async fn a_late_exit_after_the_stop_gave_up_honours_the_stop_intent() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut scenario = Scenario::new(Holder::Image, Root::Exits(ROOT_EXIT_STATUS));
    scenario.config.stop_grace = pohunek_session_worker::WorkerConfig::new()
        .terminal_commit_wait_timeout
        + LATE_COMMIT_MARGIN;
    // A worker rejects a post-kill reap deadline shorter than its grace. The
    // root here exits on its own, so the deadline never bounds this stop.
    scenario.config.kill_reap_deadline = scenario.config.stop_grace;
    let registry = scenario.registry();
    let (created, holder) = scenario.start(&registry).await;
    poll_until("the root to exit on its own", || {
        (!root_runs(created.pid)).then_some(())
    });

    let error = guard(
        "a stop that outlasts the worker's terminal-commit wait",
        Box::pin(registry.stop(&created.id)),
    )
    .await
    .expect_err("the stop gives up before the sweep lets the worker commit");
    assert_eq!(error.code, "worker_operation_failed", "{error:?}");
    assert!(
        error
            .msg
            .contains("terminal journal commit is still pending"),
        "{error:?}"
    );

    let ended = wait_until("the late exit to end the session", || async {
        let info = registry
            .inspect(&created.id)
            .await
            .expect("inspect the session");
        matches!(
            info.state,
            SessionState::Stopped | SessionState::Done | SessionState::Failed
        )
        .then_some(info)
    })
    .await;
    assert_eq!(ended.state, SessionState::Stopped, "{ended:?}");
    assert_eq!(ended.exit_code, Some(i32::from(ROOT_EXIT_STATUS)));
    assert!(ended.output_force_closed);
    // The root exited before the stop request, so only the intent can make
    // this outcome a stop.
    let journal = scenario.worker_journal();
    assert_eq!(journal["outcome"]["reason"], "natural_exit", "{journal}");
    assert!(
        !HostInspector::new()
            .is_running(holder)
            .expect("inspect the escaped holder"),
        "the late commit follows the sweep of the holder"
    );
    registry
        .remove(&created.id)
        .await
        .expect("remove the stopped session");
    drop(scenario.release);
}

/// A daemon that crashed right after it persisted a stop intent finds the
/// worker still answering with an exited runtime that it never acknowledged.
/// The restarted daemon imports that runtime as a stopped session with its
/// exit and the forced close, although the worker saw a natural exit.
///
/// It runs for about 32 s: without a stop, the worker force-closes the held
/// output only after its 30 s post-exit drain, which the real
/// `pohunek-sessiond` fixes and no configuration it reads can shorten.
#[tokio::test]
async fn an_exited_worker_still_answering_after_a_restart_honours_the_stop_intent() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let scenario = Scenario::new(Holder::Image, Root::Exits(ROOT_EXIT_STATUS));
    // The worker supervisor outlives the first daemon instance, as a service
    // manager does.
    let launcher = Arc::new(SubprocessWorkerLauncher::new());
    let (created, holder) = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("first daemon runtime");
                runtime.block_on(async {
                    let registry = scenario.registry_with(Arc::clone(&launcher));
                    let (created, holder) = scenario.start(&registry).await;
                    poll_until("the root to exit on its own", || {
                        (!root_runs(created.pid)).then_some(())
                    });
                    // The worker is still draining the PTY the holder keeps
                    // open, so the daemon goes before the runtime ends.
                    registry.begin_daemon_shutdown();
                    (created, holder)
                })
            })
            .join()
            .expect("first daemon thread")
    });
    persist_stop_intent(&scenario.store_path());
    // The drain deadline forces the output closed and the sweep ends the
    // holder; the worker then commits and keeps answering.
    let journal = poll_until("the worker's terminal commit", || {
        let journal = scenario.worker_journal();
        (journal["phase"] == "terminal").then_some(journal)
    });
    assert_eq!(journal["outcome"]["reason"], "natural_exit", "{journal}");
    assert_eq!(
        journal["outcome"]["output_forced_closed"], true,
        "{journal}"
    );

    let restarted = scenario.registry_with(launcher);
    restarted
        .reconcile_workers()
        .await
        .expect("reconcile after the restart");
    let info = restarted
        .inspect(&created.id)
        .await
        .expect("inspect the imported session");
    assert_eq!(info.state, SessionState::Stopped, "{info:?}");
    assert_eq!(info.exit_code, Some(i32::from(ROOT_EXIT_STATUS)));
    assert!(info.output_force_closed);
    assert_eq!(
        info.runtime.as_ref().map(|runtime| runtime.state),
        Some(RuntimeState::Terminal)
    );
    assert!(
        !HostInspector::new()
            .is_running(holder)
            .expect("inspect the escaped holder"),
        "the worker swept the holder before it committed"
    );
    restarted
        .remove(&created.id)
        .await
        .expect("remove the imported session");
    drop(scenario.release);
}

/// Generations the forking holder adds after its first.
///
/// The worker signals in at most three sweep passes, and each pass ends one
/// generation, so with two more generations the last one ends in the third and
/// final signalling pass.
const LAST_PASS_GENERATIONS: u8 = 2;

/// A runtime whose escaped holder forks a new marked process in answer to
/// each sweep pass is cleaned up when the last signalling pass ends the last
/// of them: the worker verifies that nothing is left before it decides.
#[tokio::test]
async fn a_holder_ended_by_the_last_signalling_pass_still_ends_the_session() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let scenario = Scenario::new(Holder::Forking(LAST_PASS_GENERATIONS), Root::Sleeps);
    let registry = scenario.registry();
    let (created, _first) = scenario.start(&registry).await;

    let stopped = guard(
        "the stop of a session whose holder forks on every sweep pass",
        Box::pin(registry.stop(&created.id)),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "a cleanup the last pass completed must not fail the stop: {error:?}\nworker sweep:\n{}",
            scenario.worker_sweep_log()
        )
    });
    assert!(stopped.stopped);

    // Every generation existed, so the sweep needed all of its passes, and
    // none is left.
    let inspector = HostInspector::new();
    let generations = scenario.env.root().join("holder-generations");
    for depth in 0..=LAST_PASS_GENERATIONS {
        let published = std::fs::read_to_string(generations.join(format!("gen-{depth}")))
            .unwrap_or_else(|error| panic!("generation {depth} never ran: {error}"));
        let pid: u32 = published.trim().parse().expect("generation PID");
        assert!(
            inspector
                .identity(pid)
                .expect("inspect a generation")
                .is_none_or(|identity| !inspector
                    .is_running(identity)
                    .expect("inspect a generation")),
            "generation {depth} survived the stop"
        );
    }
    let info = registry
        .inspect(&created.id)
        .await
        .expect("inspect the stopped session");
    assert_eq!(info.state, SessionState::Stopped);
    assert!(info.output_force_closed);
    let journal = scenario.worker_journal();
    assert_eq!(journal["phase"], "terminal", "{journal}");
    registry
        .remove(&created.id)
        .await
        .expect("remove the stopped session");
    drop(scenario.release);
}
