//! Real systemd user-manager tests for the transient-unit supervisor backend.
//!
//! Every test derives a unique installation namespace from a fresh temporary
//! directory, and a drop guard stops and resets every unit of that namespace
//! (and of any foreign unit the test started) even when an assertion fails.
//!
//! Worker units run this test binary itself as the fixture: `fixture_entry`
//! is selected by name and does real work only when the fixture marker is on
//! its command line, so the unit's main process is a `Type=notify` program
//! whose executable and argv match `ExecStart` exactly.
//!
//! The daemon and workflow plugin unit tests write into the real user
//! manager's runtime unit directory (`$XDG_RUNTIME_DIR/systemd/user`), because
//! a user manager loads persistent units only from its search path. Their unit
//! names embed the unique namespace, and the guard disables and removes them.
//! Plugin unit fixtures append `start`/`end` lines to a marker file, so the
//! tests observe every run the manager starts.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use pohunek_platform::supervisor::systemd::{
    render_daemon_unit, render_sessions_slice, unit_environment, user_manager_environment,
    SystemdDaemon, SystemdPluginUnits, SystemdSupervisor,
};
use pohunek_platform::supervisor::{
    Activation, DaemonSupervisor, Error, ExitEvidence, JobDefinition, JobExit, JobSpec, Namespace,
    PluginUnitDefinition, PluginUnitKey, PluginUnitKind, PluginUnits, RestartPolicy, ServiceId,
    ServiceObservation, ServiceState, Supervisor, WorkerKey,
};
use pohunek_test_support::wait;
use tempfile::TempDir;

mod support;

const E2E_VARIABLE: &str = "POHUNEK_SYSTEMD_E2E";
/// Command-line prefix that turns `fixture_entry` into a unit main process.
const FIXTURE_MARKER: &str = "pohunek-systemd-fixture=";
/// Bound on every D-Bus call made by the backend under test.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on waiting for a unit to reach an expected state.
const STATE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Lifetime of fixture processes; far longer than any test.
const FIXTURE_LIFETIME: Duration = Duration::from_secs(300);
/// Fixture file whose appearance tells the `allocate` fixture to start
/// allocating; named next to the state root of the installation.
const ALLOCATE_TRIGGER: &str = "allocate-now";
/// Memory limit injected into the unit of the out-of-memory scenario.
const OOM_MEMORY_MAX: &str = "64M";
/// Bytes the `allocate` fixture touches after its trigger; well above
/// [`OOM_MEMORY_MAX`].
const ALLOCATE_BYTES: usize = 512 * 1024 * 1024;
/// Start timeout of the worker whose activation never completes.
const SHORT_START_TIMEOUT: Duration = Duration::from_secs(2);
/// Session ID shared by the tests; namespaces keep them apart.
const SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN2";
/// Command-line prefix naming the marker file a plugin unit fixture appends to.
const MARKER_ARGUMENT: &str = "pohunek-systemd-marker=";
/// Command-line prefix naming the file whose appearance starts `allocate`.
const ALLOCATE_TRIGGER_ARGUMENT: &str = "pohunek-systemd-allocate-trigger=";
/// Command-line prefix naming how long a `job` fixture run sleeps, in ms.
const SLEEP_ARGUMENT: &str = "pohunek-systemd-sleep-ms=";
/// Interval of the periodic plugin job fixtures.
const JOB_INTERVAL: Duration = Duration::from_secs(1);
/// Timer accuracy of the periodic plugin job fixtures; the smallest the
/// contract accepts.
const JOB_ACCURACY: Duration = Duration::from_secs(1);
/// Runs observed before a schedule or restart loop counts as repeating.
///
/// More than systemd's default start limit (5 starts per 10 s): a restarting
/// service that keeps that limit is parked in `failed` before reaching it,
/// and a timer that keeps it skips runs until the window rolls over.
const REPEATED_RUNS: usize = 8;

fn require_e2e() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "set {E2E_VARIABLE}=1 explicitly"
    );
}

/// Unit main process used by every worker and plugin unit test.
///
/// Modes: `ready` notifies and sleeps; `never-ready` sleeps without notifying;
/// `ready-then-fail` notifies and exits with status 3; `stubborn-descendants`
/// starts a child and a grandchild that ignore `SIGHUP` and `SIGTERM`, then
/// notifies and sleeps; `exec-other` replaces itself with another program
/// before ever notifying. The plugin unit modes record a `start` line in the
/// marker file, then: `job` records its argv[0] next to the marker, sleeps the
/// given time, records `end`, and exits successfully; `crash` exits with status 3; `serve` sleeps;
/// `serve-with-descendant` also leaves a descendant in a process group of its own that records
/// each `SIGTERM` it receives in `<marker>.term` and its PID in `<marker>.descendant`, then
/// sleeps.
#[test]
#[ignore = "fixture entry point; does work only inside a unit started by these tests"]
fn fixture_entry() {
    let Some(mode) = std::env::args()
        .find_map(|argument| argument.strip_prefix(FIXTURE_MARKER).map(str::to_owned))
    else {
        return;
    };
    match mode.as_str() {
        "ready" => {
            notify_ready();
            std::thread::sleep(FIXTURE_LIFETIME);
        }
        "never-ready" => std::thread::sleep(FIXTURE_LIFETIME),
        "exec-other" => {
            use std::os::unix::process::CommandExt as _;

            let error = Command::new("/usr/bin/sleep")
                .arg(FIXTURE_LIFETIME.as_secs().to_string())
                .exec();
            panic!("exec another program: {error}");
        }
        "ready-then-sigkill" => {
            notify_ready();
            nix::sys::signal::kill(nix::unistd::getpid(), nix::sys::signal::Signal::SIGKILL)
                .expect("kill the fixture's own process");
        }
        "allocate" => {
            notify_ready();
            let trigger = PathBuf::from(argument(ALLOCATE_TRIGGER_ARGUMENT));
            while !trigger.exists() {
                // timing-allowed: #326 the fixture waits for the scenario to inject the memory limit; the scenario waits on the unit's state, not on this poll
                std::thread::sleep(POLL_INTERVAL);
            }
            let mut memory = vec![0_u8; ALLOCATE_BYTES];
            for page in memory.chunks_mut(4_096) {
                page[0] = 1;
            }
            std::hint::black_box(&memory);
            // timing-allowed: #326 the fixture's own lifetime; the manager ends the unit
            std::thread::sleep(FIXTURE_LIFETIME);
        }
        "ready-then-fail" => {
            notify_ready();
            std::process::exit(3);
        }
        "stubborn-descendants" => {
            let mut descendants = Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap '' HUP TERM; /usr/bin/sleep 300 & exec /usr/bin/sleep 300",
                ])
                .spawn()
                .expect("spawn stubborn descendants");
            notify_ready();
            std::thread::sleep(FIXTURE_LIFETIME);
            descendants.kill().expect("kill stubborn child");
            descendants.wait().expect("reap stubborn child");
        }
        "job" => {
            let marker = record_run("start");
            let argv0 = std::env::args_os().next().expect("argv[0]");
            std::fs::write(argv0_record(&marker), argv0.as_encoded_bytes())
                .expect("record argv[0]");
            let sleep = argument(SLEEP_ARGUMENT)
                .parse()
                .expect("numeric sleep milliseconds");
            // timing-allowed: #326 the run's own length, which the scenarios measure real systemd timers against; no scenario waits on this sleep
            std::thread::sleep(Duration::from_millis(sleep));
            append_line(&marker, "end");
        }
        "crash" => {
            record_run("start");
            std::process::exit(3);
        }
        "serve-with-descendant" => {
            use std::os::unix::process::CommandExt as _;

            let marker = record_run("start");
            // A descendant in a process group of its own that records every
            // `SIGTERM` it receives and outlives the fixture unless killed.
            // timing-allowed: #326 the descendant's own lifetime; no scenario waits on this sleep
            let mut descendant = Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap 'echo term >> \"$0\"' TERM; /usr/bin/sleep 300 & wait; wait",
                    &format!("{}.term", marker.display()),
                ])
                .process_group(0)
                .spawn()
                .expect("spawn the descendant");
            std::fs::write(
                format!("{}.descendant", marker.display()),
                descendant.id().to_string(),
            )
            .expect("record the descendant");
            // timing-allowed: #326 the fixture's own lifetime; no scenario waits on this sleep, it ends when the scenario stops the unit
            std::thread::sleep(FIXTURE_LIFETIME);
            descendant.kill().expect("kill the descendant");
            descendant.wait().expect("reap the descendant");
        }
        "serve" => {
            record_run("start");
            // timing-allowed: #326 the fixture's own lifetime; no scenario waits on this sleep, it ends when the scenario stops the unit
            std::thread::sleep(FIXTURE_LIFETIME);
        }
        other => panic!("unknown fixture mode {other}"),
    }
}

/// Returns the file a `job` fixture records its argv[0] in.
fn argv0_record(marker: &Path) -> PathBuf {
    let mut record = marker.as_os_str().to_owned();
    record.push(".argv0");
    PathBuf::from(record)
}

fn argument(prefix: &str) -> String {
    std::env::args()
        .find_map(|argument| argument.strip_prefix(prefix).map(str::to_owned))
        .unwrap_or_else(|| panic!("fixture argument {prefix} is missing"))
}

/// Appends one `<kind> <pid> <unix-nanos>` line to the marker file and
/// returns its path.
fn record_run(kind: &str) -> PathBuf {
    let marker = PathBuf::from(argument(MARKER_ARGUMENT));
    append_line(&marker, kind);
    marker
}

fn append_line(marker: &Path, kind: &str) {
    use std::io::Write as _;

    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    // One `write` of a short line to an `O_APPEND` file is atomic, so lines of
    // concurrent runs could interleave only as whole lines.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
        .expect("open marker file")
        .write_all(format!("{kind} {} {at}\n", std::process::id()).as_bytes())
        .expect("append marker line");
}

fn notify_ready() {
    use std::os::linux::net::SocketAddrExt as _;

    let target = std::env::var("NOTIFY_SOCKET").expect("NOTIFY_SOCKET is set by systemd");
    let socket = UnixDatagram::unbound().expect("notify socket");
    let address = match target.strip_prefix('@') {
        Some(name) => std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .expect("abstract notify address"),
        None => std::os::unix::net::SocketAddr::from_pathname(&target).expect("notify socket path"),
    };
    socket
        .send_to_addr(b"READY=1", &address)
        .expect("send READY=1");
}

/// Stops, resets, disables, and removes everything a test created.
struct Cleanup {
    patterns: Vec<String>,
    unit_dir: Option<(PathBuf, Vec<String>)>,
}

impl Cleanup {
    fn new(namespace: &Namespace) -> Self {
        Self {
            // Children first: the sessions slice nests under the implicit
            // `pohunek-<ns>.slice`, which lingers once created.
            patterns: vec![
                format!("pohunek-{namespace}-*"),
                format!("pohunek-{namespace}.slice"),
            ],
            unit_dir: None,
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pattern in &self.patterns {
            systemctl(&["stop", pattern]);
            systemctl(&["reset-failed", pattern]);
        }
        if let Some((directory, units)) = &self.unit_dir {
            for unit in units {
                systemctl(&["disable", unit]);
                match std::fs::remove_file(directory.join(unit)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => eprintln!("cleanup could not remove {unit}: {error}"),
                }
            }
            systemctl(&["daemon-reload"]);
        }
    }
}

// The systemd commands below keep the inherited environment on purpose: the
// session bus address and `XDG_RUNTIME_DIR` select the real user manager that
// these suites exercise, and the suites only run when explicitly enabled.
fn systemctl(arguments: &[&str]) {
    match Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => eprintln!(
            "cleanup `systemctl --user {}` exited with {}",
            arguments.join(" "),
            output.status
        ),
        Err(error) => eprintln!("cleanup could not run systemctl: {error}"),
    }
}

/// Runs `systemctl --user` and fails the scenario when it does not succeed.
fn systemctl_checked(arguments: &[&str]) {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()
        .expect("run systemctl");
    assert!(
        output.status.success(),
        "`systemctl --user {}` failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A namespace unique to one test and the directory it was derived from.
struct Installation {
    root: TempDir,
    namespace: Namespace,
    cleanup: Cleanup,
}

impl Installation {
    fn new() -> Self {
        let root = pohunek_test_support::tempdir().expect("temporary installation root");
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&state).expect("state root");
        std::fs::create_dir_all(&runtime).expect("runtime root");
        let namespace = Namespace::derive(nix::unistd::getuid().as_raw(), &state, &runtime);
        let cleanup = Cleanup::new(&namespace);
        Self {
            root,
            namespace,
            cleanup,
        }
    }

    async fn supervisor(&self) -> SystemdSupervisor {
        SystemdSupervisor::connect(self.namespace.clone(), CALL_TIMEOUT)
            .await
            .expect("connect to the systemd user manager")
    }

    fn unit(&self, id: &ServiceId) -> String {
        self.namespace
            .worker_unit(&WorkerKey::from_service_id(id).expect("worker id"))
    }
}

fn worker_id(generation: &str) -> ServiceId {
    WorkerKey::new(SESSION, generation)
        .expect("valid worker key")
        .service_id()
}

fn fixture_spec(root: &Path, mode: &str) -> JobSpec {
    let exact = pohunek_test_support::exact_test_filter(module_path!(), "fixture_entry");
    JobSpec {
        executable: std::env::current_exe().expect("test executable"),
        arguments: vec![
            exact[0].clone(),
            exact[1].clone(),
            "--ignored".to_owned(),
            "--test-threads=1".to_owned(),
            format!("{FIXTURE_MARKER}{mode}"),
        ],
        environment: BTreeMap::from([(
            "XDG_STATE_HOME".to_owned(),
            root.join("state").to_string_lossy().into_owned(),
        )]),
        working_directory: root.to_path_buf(),
        logs: None,
        start_timeout: Duration::from_secs(60),
        exit_timeout: Duration::from_secs(5),
        restart: RestartPolicy::Never,
        open_files: 4_096,
    }
}

fn fixture(root: &Path, mode: &str) -> JobDefinition {
    JobDefinition::new(fixture_spec(root, mode)).expect("valid fixture definition")
}

/// Polls `inspect` until `accept` holds, treating races as transient.
async fn wait_for(
    supervisor: &SystemdSupervisor,
    id: &ServiceId,
    accept: impl Fn(&ServiceObservation) -> bool,
) -> ServiceObservation {
    let deadline = Instant::now() + STATE_TIMEOUT;
    loop {
        match supervisor.inspect(id).await {
            Ok(observation) if accept(&observation) => return observation,
            Ok(_) | Err(Error::Race { .. }) => {}
            Err(error) => panic!("inspect {id} failed: {error:?}"),
        }
        assert!(Instant::now() < deadline, "{id} never reached the state");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn show(unit: &str, properties: &[&str]) -> BTreeMap<String, String> {
    let output = Command::new("systemctl")
        .args(["--user", "show", unit, "--property"])
        .arg(properties.join(","))
        .output()
        .expect("run systemctl show");
    assert!(output.status.success(), "systemctl show {unit} failed");
    String::from_utf8(output.stdout)
        .expect("UTF-8 properties")
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn run_foreign_unit(unit: &str) {
    let status = Command::new("systemd-run")
        .args(["--user", "--no-block", "--quiet", "--unit", unit])
        .args(["/usr/bin/sleep", "300"])
        .status()
        .expect("run systemd-run");
    assert!(status.success(), "systemd-run {unit} failed");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn worker_generation_starts_inspects_and_retires() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("abcd2345");
    let definition = fixture(installation.root.path(), "ready");

    supervisor
        .start(&id, &definition)
        .await
        .expect("start transient worker");
    let observation = wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert_eq!(observation.definition, Some(definition.facts()));
    let process = observation.process.expect("running worker has a process");
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", process.pid)).expect("worker executable"),
        std::fs::canonicalize(definition.executable()).expect("canonical executable")
    );

    let unit = installation.unit(&id);
    let properties = show(
        &unit,
        &[
            "Type",
            "NotifyAccess",
            "KillMode",
            "SendSIGHUP",
            "Slice",
            "Restart",
            "TimeoutStartUSec",
            "TimeoutStopUSec",
            "Environment",
            "WorkingDirectory",
            "LimitNOFILE",
            "LimitNOFILESoft",
            "StandardOutput",
            "StandardError",
            "Transient",
        ],
    );
    let expected = [
        ("Type", "notify".to_owned()),
        ("NotifyAccess", "main".to_owned()),
        ("KillMode", "control-group".to_owned()),
        ("SendSIGHUP", "yes".to_owned()),
        ("Slice", installation.namespace.sessions_slice()),
        ("Restart", "no".to_owned()),
        ("TimeoutStartUSec", "1min".to_owned()),
        ("TimeoutStopUSec", "5s".to_owned()),
        (
            "Environment",
            format!(
                "XDG_STATE_HOME={}",
                installation.root.path().join("state").display()
            ),
        ),
        (
            "WorkingDirectory",
            installation.root.path().display().to_string(),
        ),
        ("LimitNOFILE", "4096".to_owned()),
        ("LimitNOFILESoft", "4096".to_owned()),
        ("StandardOutput", "journal".to_owned()),
        ("StandardError", "journal".to_owned()),
        ("Transient", "yes".to_owned()),
    ];
    for (key, value) in expected {
        assert_eq!(properties.get(key), Some(&value), "property {key}");
    }

    assert!(matches!(
        supervisor.start(&id, &definition).await,
        Err(Error::AlreadyRegistered(found)) if found == id
    ));
    let discovered = supervisor.discover().await.expect("discover workers");
    assert_eq!(
        discovered
            .iter()
            .map(|observation| &observation.id)
            .collect::<Vec<_>>(),
        vec![&id]
    );

    supervisor.retire(&id).await.expect("retire worker");
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::NotFound(found)) if found == id
    ));
    assert!(matches!(
        supervisor.retire(&id).await,
        Err(Error::NotFound(found)) if found == id
    ));
    assert!(
        std::fs::metadata(format!("/proc/{}", process.pid)).is_err(),
        "worker process outlived retirement"
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn dollar_signs_reach_the_worker_verbatim() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("abcd2345");
    // systemd expands `$VAR`, `${VAR}`, and `$$` in command lines unless told
    // not to; the executable path carries the same hazard through argv[0].
    let directory = installation.root.path().join("bin$HOME${HOME}$$");
    std::fs::create_dir(&directory).expect("dollar-bearing directory");
    let executable = directory.join("fixture");
    std::fs::copy(
        std::env::current_exe().expect("test executable"),
        &executable,
    )
    .expect("copy fixture executable");
    let mut spec = fixture_spec(installation.root.path(), "ready");
    spec.executable = executable;
    spec.arguments.push("literal-$HOME-${HOME}-$$".to_owned());
    let definition = JobDefinition::new(spec).expect("valid fixture definition");

    supervisor
        .start(&id, &definition)
        .await
        .expect("start transient worker");
    let observation = wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert_eq!(observation.definition, Some(definition.facts()));
    let process = observation.process.expect("running worker has a process");
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", process.pid)).expect("cmdline");
    let expected: Vec<u8> = std::iter::once(definition.executable().as_os_str().as_encoded_bytes())
        .chain(definition.arguments().iter().map(String::as_bytes))
        .flat_map(|argument| argument.iter().copied().chain([0]))
        .collect();
    assert_eq!(cmdline, expected);
    let discovered = supervisor.discover().await.expect("discover workers");
    assert_eq!(discovered.len(), 1);

    supervisor.retire(&id).await.expect("retire worker");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_never_ready_worker_is_retired_while_starting() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("neverrdy");

    supervisor
        .start(&id, &fixture(installation.root.path(), "never-ready"))
        .await
        .expect("start transient worker");
    let observation = wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Starting && observation.process.is_some()
    })
    .await;
    assert!(observation.definition.is_some());

    supervisor
        .retire(&id)
        .await
        .expect("retire a starting worker");
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_starting_unit_not_running_exec_start_is_observed_without_a_process() {
    /// Consecutive inspections that must all succeed unbound.
    const STABLE_INSPECTIONS: usize = 5;

    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("execothr");

    supervisor
        .start(&id, &fixture(installation.root.path(), "exec-other"))
        .await
        .expect("start transient worker");
    wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Starting
            && observation.process.is_none()
            && show(&installation.unit(&id), &["MainPID"])
                .get("MainPID")
                .is_some_and(|pid| pid != "0")
    })
    .await;
    for _ in 0..STABLE_INSPECTIONS {
        let observation = supervisor
            .inspect(&id)
            .await
            .expect("a starting unit is observed, not raced");
        assert_eq!(observation.state, ServiceState::Starting);
        assert_eq!(observation.process, None, "another image is never bound");
        assert!(observation.definition.is_some());
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    supervisor
        .retire(&id)
        .await
        .expect("retire a starting worker");
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn retire_kills_hangup_ignoring_descendants_after_the_stop_timeout() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("stubborn");
    let mut spec = fixture_spec(installation.root.path(), "stubborn-descendants");
    spec.exit_timeout = Duration::from_secs(2);
    let definition = JobDefinition::new(spec).expect("valid definition");

    supervisor
        .start(&id, &definition)
        .await
        .expect("start transient worker");
    wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    let control_group = show(&installation.unit(&id), &["ControlGroup"])
        .remove("ControlGroup")
        .expect("control group");
    let deadline = Instant::now() + STATE_TIMEOUT;
    let pids = loop {
        let pids: Vec<u32> =
            std::fs::read_to_string(format!("/sys/fs/cgroup{control_group}/cgroup.procs"))
                .expect("read cgroup processes")
                .lines()
                .map(|line| line.parse().expect("numeric PID"))
                .collect();
        if pids.len() >= 3 {
            break pids;
        }
        assert!(Instant::now() < deadline, "descendants never started");
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    let started = Instant::now();
    supervisor.retire(&id).await.expect("retire worker");
    assert!(
        started.elapsed() >= definition.exit_timeout(),
        "descendants ignoring SIGTERM must survive until TimeoutStopSec"
    );
    for pid in pids {
        assert!(
            std::fs::metadata(format!("/proc/{pid}")).is_err(),
            "process {pid} outlived retirement"
        );
    }
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_failed_worker_is_observed_and_reset_by_retire() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("failed22");
    let definition = fixture(installation.root.path(), "ready-then-fail");

    supervisor
        .start(&id, &definition)
        .await
        .expect("start transient worker");
    let observation = wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Failed
    })
    .await;
    assert_eq!(observation.process, None);
    assert_eq!(observation.definition, Some(definition.facts()));

    supervisor.retire(&id).await.expect("retire failed worker");
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::NotFound(_))
    ));
}

/// Polls `exit_evidence` until the failed unit reports a verdict.
async fn failed_evidence(supervisor: &SystemdSupervisor, id: &ServiceId) -> ExitEvidence {
    let deadline = Instant::now() + STATE_TIMEOUT;
    loop {
        match supervisor.exit_evidence(id).await {
            Ok(Some(evidence)) => return evidence,
            Ok(None) | Err(Error::Race { .. }) => {}
            Err(error) => panic!("exit_evidence {id} failed: {error:?}"),
        }
        assert!(Instant::now() < deadline, "{id} never reported a verdict");
        // timing-allowed: #326 polls the real manager, which publishes the failed state asynchronously; bounded by STATE_TIMEOUT
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn exit_evidence_is_absent_for_a_missing_or_running_worker() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("verdicta");

    assert!(matches!(
        supervisor.exit_evidence(&id).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        supervisor.exit_status(&id).await,
        Err(Error::NotFound(_))
    ));

    supervisor
        .start(&id, &fixture(installation.root.path(), "ready"))
        .await
        .expect("start transient worker");
    wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert_eq!(supervisor.exit_evidence(&id).await.expect("running"), None);
    assert_eq!(supervisor.exit_status(&id).await.expect("running"), None);
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_failed_worker_reports_its_exit_code_until_it_is_retired() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("verdict2");

    supervisor
        .start(&id, &fixture(installation.root.path(), "ready-then-fail"))
        .await
        .expect("start transient worker");
    let evidence = failed_evidence(&supervisor, &id).await;

    assert_eq!(evidence.result(), "exit-code");
    assert_eq!(evidence.exit(), Some(JobExit::Code(3)));
    assert_eq!(
        supervisor.exit_status(&id).await.expect("exit status"),
        Some(JobExit::Code(3))
    );

    supervisor.retire(&id).await.expect("retire failed worker");
    assert!(
        matches!(supervisor.exit_evidence(&id).await, Err(Error::NotFound(_))),
        "retiring a failed unit resets it and destroys the record"
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_worker_killed_by_a_signal_reports_the_signal() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("verdict3");

    supervisor
        .start(
            &id,
            &fixture(installation.root.path(), "ready-then-sigkill"),
        )
        .await
        .expect("start transient worker");
    let evidence = failed_evidence(&supervisor, &id).await;

    assert_eq!(evidence.result(), "signal");
    assert_eq!(
        evidence.exit(),
        Some(JobExit::Signal(nix::sys::signal::Signal::SIGKILL as i32))
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_worker_that_never_becomes_ready_reports_a_timeout() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("verdict4");
    let mut spec = fixture_spec(installation.root.path(), "never-ready");
    spec.start_timeout = SHORT_START_TIMEOUT;

    supervisor
        .start(&id, &JobDefinition::new(spec).expect("valid definition"))
        .await
        .expect("start transient worker");
    let evidence = failed_evidence(&supervisor, &id).await;

    assert_eq!(evidence.result(), "timeout");
}

/// Fails with a stated precondition unless the unit's cgroup enforces a
/// memory limit, which needs the memory controller delegated to the user
/// manager.
fn require_memory_controller(unit: &str) {
    let properties = show(unit, &["ControlGroup"]);
    let group = properties
        .get("ControlGroup")
        .filter(|group| !group.is_empty())
        .unwrap_or_else(|| panic!("precondition: {unit} has no control group"));
    let limit = Path::new("/sys/fs/cgroup")
        .join(group.trim_start_matches('/'))
        .join("memory.max");
    let value = std::fs::read_to_string(&limit).unwrap_or_default();
    assert!(
        value.trim() != "" && value.trim() != "max",
        "precondition: the systemd user manager does not enforce MemoryMax for {unit}; \
         delegate the memory controller to the user manager (Delegate=memory on \
         user@.service) before running this scenario"
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager with the memory controller"]
async fn a_worker_under_memory_pressure_reports_an_oom_kill() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("verdict5");
    let trigger = installation.root.path().join(ALLOCATE_TRIGGER);
    let mut spec = fixture_spec(installation.root.path(), "allocate");
    spec.arguments
        .push(format!("{ALLOCATE_TRIGGER_ARGUMENT}{}", trigger.display()));

    supervisor
        .start(&id, &JobDefinition::new(spec).expect("valid definition"))
        .await
        .expect("start transient worker");
    wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    let unit = installation.unit(&id);
    systemctl_checked(&[
        "set-property",
        "--runtime",
        &unit,
        &format!("MemoryMax={OOM_MEMORY_MAX}"),
        "MemorySwapMax=0",
    ]);
    require_memory_controller(&unit);
    std::fs::write(&trigger, b"go").expect("trigger the allocation");

    let evidence = failed_evidence(&supervisor, &id).await;

    assert_eq!(evidence.result(), "oom-kill");
    assert!(
        matches!(evidence.exit(), Some(JobExit::Signal(_))),
        "the main process died of a signal: {evidence:?}"
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn discovery_ignores_foreign_malformed_and_other_namespace_units() {
    require_e2e();
    let installation = Installation::new();
    let other = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("abcd2345");
    supervisor
        .start(&id, &fixture(installation.root.path(), "ready"))
        .await
        .expect("start transient worker");

    let other_unit = other.unit(&id);
    let malformed_unit = format!("pohunek-{}-worker-garbage.service", installation.namespace);
    let foreign_unit = format!("pohunek-{}-foreign.service", installation.namespace);
    run_foreign_unit(&other_unit);
    run_foreign_unit(&malformed_unit);
    run_foreign_unit(&foreign_unit);
    wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;

    let discovery = supervisor.discover_units().await.expect("discover workers");
    assert_eq!(
        discovery
            .observations
            .iter()
            .map(|observation| &observation.id)
            .collect::<Vec<_>>(),
        vec![&id]
    );
    assert_eq!(discovery.rejected_units, vec![malformed_unit.clone()]);
    let captured = support::LogCapture::default();
    let observations = {
        let _capture = captured.install();
        supervisor.discover().await.expect("trait discovery")
    };
    assert_eq!(observations.len(), 1);
    assert_eq!(
        captured.rejected_entries(),
        vec![malformed_unit.clone()],
        "the trait discovery warns about every rejected unit"
    );

    supervisor.retire(&id).await.expect("retire worker");
    for unit in [&other_unit, &malformed_unit, &foreign_unit] {
        assert_eq!(
            show(unit, &["ActiveState"])
                .get("ActiveState")
                .map(String::as_str),
            Some("active"),
            "{unit} was touched by another namespace's supervisor"
        );
    }
    drop(other);
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn inspect_rejects_a_main_process_that_is_not_exec_start() {
    require_e2e();
    let installation = Installation::new();
    let supervisor = installation.supervisor().await;
    let id = worker_id("execswap");
    let mut spec = fixture_spec(installation.root.path(), "unused");
    spec.executable = PathBuf::from("/usr/bin/systemd-notify");
    spec.arguments = ["--ready", "--exec", ";", "/usr/bin/sleep", "300"]
        .map(str::to_owned)
        .to_vec();
    supervisor
        .start(&id, &JobDefinition::new(spec).expect("valid definition"))
        .await
        .expect("start transient worker");

    let unit = installation.unit(&id);
    let deadline = Instant::now() + STATE_TIMEOUT;
    while show(&unit, &["ActiveState"])
        .get("ActiveState")
        .map(String::as_str)
        != Some("active")
    {
        assert!(Instant::now() < deadline, "{unit} never became active");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::InvalidData {
            operation: "inspect",
            ..
        })
    ));
    supervisor.retire(&id).await.expect("retire worker");
}

/// Returns the user manager's runtime unit directory.
///
/// `$XDG_RUNTIME_DIR/systemd/user` is on every user manager's search path,
/// lives under the private runtime root, and vanishes at logout, so the test
/// never depends on the permissions of the operator's `~/.config`.
fn user_unit_dir() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .expect("XDG_RUNTIME_DIR is set for a user manager");
    runtime.join("systemd/user")
}

fn analyze_verify(path: &Path) {
    let output = Command::new("systemd-analyze")
        .args(["--user", "verify"])
        .arg(path)
        .output()
        .expect("run systemd-analyze");
    assert!(
        output.status.success(),
        "systemd-analyze verify {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn daemon_unit_installs_replaces_inspects_and_uninstalls() {
    require_e2e();
    let mut installation = Installation::new();
    let unit_dir = user_unit_dir();
    let daemon_unit = installation.namespace.daemon_unit();
    let slice = installation.namespace.sessions_slice();
    installation.cleanup.unit_dir =
        Some((unit_dir.clone(), vec![daemon_unit.clone(), slice.clone()]));

    let mut spec = fixture_spec(installation.root.path(), "ready");
    spec.restart = RestartPolicy::OnFailure {
        throttle: Duration::from_secs(1),
    };
    // `WorkingDirectory=` takes backslashes and quotes verbatim; the manager's
    // parsed value proves the rendering round-trips them and `%`.
    let working_directory = installation
        .root
        .path()
        .join("wd a\\s b\\\\c \"q\" 'r' 100%");
    std::fs::create_dir(&working_directory).expect("working directory");
    spec.working_directory = working_directory.clone();
    // systemd substitutes `$` in argv[0] unless the command opts out, and the
    // daemon's observations require argv[0] to equal the recorded executable.
    let bin = installation.root.path().join("bin${HOME}$$%h");
    std::fs::create_dir(&bin).expect("dollar-bearing directory");
    spec.executable = bin.join("fixture");
    std::fs::copy(
        std::env::current_exe().expect("test executable"),
        &spec.executable,
    )
    .expect("copy fixture executable");
    let definition = JobDefinition::new(spec.clone()).expect("valid definition");
    let rendered_dir = installation.root.path().join("rendered");
    std::fs::create_dir(&rendered_dir).expect("render directory");
    std::fs::write(
        rendered_dir.join(&daemon_unit),
        render_daemon_unit(&definition).expect("render daemon unit"),
    )
    .expect("write rendered unit");
    std::fs::write(rendered_dir.join(&slice), render_sessions_slice()).expect("write slice");
    analyze_verify(&rendered_dir.join(&daemon_unit));
    analyze_verify(&rendered_dir.join(&slice));

    let daemon = SystemdDaemon::connect(
        installation.namespace.clone(),
        unit_dir.clone(),
        CALL_TIMEOUT,
    )
    .await
    .expect("connect to the systemd user manager");
    assert!(matches!(daemon.inspect().await, Err(Error::NotFound(_))));
    let replaced = daemon.replace(&definition).await;
    assert!(matches!(replaced, Err(Error::NotFound(_))), "{replaced:?}");
    // A daemon restarting after a clean exit could never stop itself.
    let mut always = spec.clone();
    always.restart = RestartPolicy::Always {
        delay: Duration::from_secs(1),
    };
    let always = JobDefinition::new(always).expect("valid definition");
    assert!(matches!(
        daemon.install(&always).await,
        Err(Error::InvalidDefinition { .. })
    ));
    assert!(!unit_dir.join(&daemon_unit).exists());

    daemon.install(&definition).await.expect("install daemon");
    let first = wait_for_daemon(&daemon, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert_eq!(first.id.as_str(), daemon_unit);
    assert_eq!(first.definition, Some(definition.facts()));
    assert_eq!(
        show(&daemon_unit, &["WorkingDirectory"])
            .get("WorkingDirectory")
            .map(String::as_str),
        working_directory.to_str()
    );
    assert_eq!(unit_file_state(&daemon_unit), "enabled");
    let accounting = show(&slice, &["MemoryAccounting", "TasksAccounting"]);
    for key in ["MemoryAccounting", "TasksAccounting"] {
        assert_eq!(
            accounting.get(key).map(String::as_str),
            Some("yes"),
            "{key}"
        );
    }
    assert!(matches!(
        daemon.install(&definition).await,
        Err(Error::AlreadyRegistered(_))
    ));

    spec.arguments.push("replaced-definition".to_owned());
    let replacement = JobDefinition::new(spec).expect("valid replacement");
    daemon.replace(&replacement).await.expect("replace daemon");
    let first_pid = first.process.expect("running daemon").pid;
    let second = wait_for_daemon(&daemon, |observation| {
        observation.state == ServiceState::Running
            && observation.definition == Some(replacement.facts())
            && observation
                .process
                .is_some_and(|process| process.pid != first_pid)
    })
    .await;
    assert!(second.process.is_some());

    daemon.uninstall().await.expect("uninstall daemon");
    assert!(matches!(daemon.inspect().await, Err(Error::NotFound(_))));
    assert!(!unit_dir.join(&daemon_unit).exists());
    assert!(!unit_dir.join(&slice).exists());
    daemon.uninstall().await.expect("uninstall is idempotent");
}

/// Returns `systemctl --user is-enabled` for one unit, such as `enabled`.
fn unit_file_state(unit: &str) -> String {
    let output = Command::new("systemctl")
        .args(["--user", "is-enabled", unit])
        .output()
        .expect("run systemctl is-enabled");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn replace_enables_a_written_but_disabled_daemon_unit() {
    require_e2e();
    let mut installation = Installation::new();
    let unit_dir = user_unit_dir();
    let daemon_unit = installation.namespace.daemon_unit();
    installation.cleanup.unit_dir = Some((
        unit_dir.clone(),
        vec![daemon_unit.clone(), installation.namespace.sessions_slice()],
    ));
    let mut spec = fixture_spec(installation.root.path(), "ready");
    spec.restart = RestartPolicy::OnFailure {
        throttle: Duration::from_secs(1),
    };
    let definition = JobDefinition::new(spec).expect("valid definition");
    let daemon = SystemdDaemon::connect(installation.namespace.clone(), unit_dir, CALL_TIMEOUT)
        .await
        .expect("connect to the systemd user manager");
    daemon.install(&definition).await.expect("install daemon");
    wait_for_daemon(&daemon, |observation| {
        observation.state == ServiceState::Running
    })
    .await;

    // An install interrupted between writing the unit and enabling it leaves
    // exactly this state: the unit file present but not enabled for login.
    let disabled = Command::new("systemctl")
        .args(["--user", "disable", &daemon_unit])
        .output()
        .expect("run systemctl disable");
    assert!(
        disabled.status.success(),
        "systemctl disable {daemon_unit} failed: {}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert_eq!(unit_file_state(&daemon_unit), "disabled");

    daemon.replace(&definition).await.expect("replace daemon");
    assert_eq!(unit_file_state(&daemon_unit), "enabled");
    wait_for_daemon(&daemon, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert_eq!(
        show(&daemon_unit, &["ActiveState"])["ActiveState"],
        "active"
    );

    daemon.uninstall().await.expect("uninstall daemon");
}

async fn wait_for_daemon(
    daemon: &SystemdDaemon,
    accept: impl Fn(&ServiceObservation) -> bool,
) -> ServiceObservation {
    let deadline = Instant::now() + STATE_TIMEOUT;
    loop {
        match daemon.inspect().await {
            Ok(observation) if accept(&observation) => return observation,
            Ok(_) | Err(Error::Race { .. } | Error::NotFound(_)) => {}
            Err(error) => panic!("inspect daemon failed: {error:?}"),
        }
        assert!(Instant::now() < deadline, "daemon never reached the state");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn daemon_uninstall_stops_only_empty_namespace_slices() {
    require_e2e();
    let mut installation = Installation::new();
    let unit_dir = user_unit_dir();
    let slice = installation.namespace.sessions_slice();
    let parent = format!("pohunek-{}.slice", installation.namespace.as_str());
    installation.cleanup.unit_dir = Some((
        unit_dir.clone(),
        vec![installation.namespace.daemon_unit(), slice.clone()],
    ));
    let supervisor = installation.supervisor().await;
    let daemon = SystemdDaemon::connect(installation.namespace.clone(), unit_dir, CALL_TIMEOUT)
        .await
        .expect("connect to the systemd user manager");
    let id = worker_id("slce2345");
    supervisor
        .start(&id, &fixture(installation.root.path(), "ready"))
        .await
        .expect("start transient worker");
    wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;

    daemon
        .uninstall()
        .await
        .expect("uninstall with a live worker");
    let running = wait_for(&supervisor, &id, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert!(
        running.process.is_some(),
        "uninstall must not touch workers"
    );
    assert_eq!(show(&slice, &["ActiveState"])["ActiveState"], "active");

    supervisor.retire(&id).await.expect("retire worker");
    daemon
        .uninstall()
        .await
        .expect("uninstall after the last worker");
    let deadline = Instant::now() + STATE_TIMEOUT;
    for unit in [&slice, &parent] {
        while show(unit, &["ActiveState"])["ActiveState"] != "inactive" {
            assert!(Instant::now() < deadline, "{unit} stayed active");
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

/// One line of a plugin unit fixture's marker file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    start: bool,
    pid: u32,
    at: u128,
}

fn read_marks(marker: &Path) -> Vec<Mark> {
    match std::fs::read_to_string(marker) {
        Ok(text) => text
            .lines()
            .map(|line| {
                let mut fields = line.split(' ');
                let (Some(kind), Some(pid), Some(at), None) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    panic!("malformed marker line {line:?}");
                };
                Mark {
                    start: match kind {
                        "start" => true,
                        "end" => false,
                        other => panic!("unknown marker kind {other}"),
                    },
                    pid: pid.parse().expect("numeric PID"),
                    at: at.parse().expect("numeric timestamp"),
                }
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read marker file: {error}"),
    }
}

/// Waits until `accept` holds for the marker file's lines.
async fn wait_for_marks(marker: &Path, accept: impl Fn(&[Mark]) -> bool) -> Vec<Mark> {
    wait::wait_until("the expected fixture runs in the marker file", || {
        let marks = read_marks(marker);
        let accepted = accept(&marks);
        async move { accepted.then_some(marks) }
    })
    .await
}

fn count(marks: &[Mark], start: bool) -> usize {
    marks.iter().filter(|mark| mark.start == start).count()
}

/// A namespace plus the plugin unit backend writing into the user manager's
/// runtime unit directory.
struct PluginInstallation {
    installation: Installation,
    units: SystemdPluginUnits,
    unit_dir: PathBuf,
}

impl PluginInstallation {
    async fn new() -> Self {
        let installation = Installation::new();
        let unit_dir = user_unit_dir();
        let units = SystemdPluginUnits::connect(
            installation.namespace.clone(),
            unit_dir.clone(),
            CALL_TIMEOUT,
        )
        .await
        .expect("connect to the systemd user manager");
        Self {
            installation,
            units,
            unit_dir,
        }
    }

    /// Registers the unit set of `key` with the cleanup guard and returns
    /// its service and timer unit names.
    fn track(&mut self, key: &PluginUnitKey) -> (String, String) {
        let namespace = &self.installation.namespace;
        let service = namespace.plugin_service_unit(key);
        let timer = namespace.plugin_timer_unit(key);
        let (_, units) = self
            .installation
            .cleanup
            .unit_dir
            .get_or_insert_with(|| (self.unit_dir.clone(), Vec::new()));
        units.extend([timer.clone(), service.clone()]);
        (service, timer)
    }

    fn root(&self) -> &Path {
        self.installation.root.path()
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.root().join(format!("{name}.marks"))
    }

    /// Returns every loaded unit, active or not, of this namespace's plugin
    /// units.
    fn loaded_plugin_units(&self) -> Vec<String> {
        let output = Command::new("systemctl")
            .args(["--user", "list-units", "--all", "--plain", "--no-legend"])
            .arg(format!("pohunek-{}-wf-*", self.installation.namespace))
            .output()
            .expect("run systemctl list-units");
        assert!(output.status.success(), "systemctl list-units failed");
        String::from_utf8(output.stdout)
            .expect("UTF-8 unit list")
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect()
    }

    /// Returns the files of this namespace's plugin units in the unit
    /// directory.
    fn plugin_unit_files(&self) -> Vec<String> {
        let prefix = format!("pohunek-{}-wf-", self.installation.namespace);
        let mut files: Vec<String> = std::fs::read_dir(&self.unit_dir)
            .expect("read unit directory")
            .map(|entry| {
                entry
                    .expect("unit directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(&prefix))
            .collect();
        files.sort();
        files
    }

    async fn wait_for_state(&self, key: &PluginUnitKey, accept: impl Fn(ServiceState) -> bool) {
        let accept = &accept;
        wait::wait_until("the plugin unit state", move || async move {
            let state = self.units.state(key).await.expect("plugin unit state");
            accept(state).then_some(())
        })
        .await;
    }
}

/// A plugin unit job running a fixture mode with bootstrap-only environment.
fn plugin_job(root: &Path, mode: &str, marker: &Path, sleep: Duration) -> JobSpec {
    let mut spec = fixture_spec(root, mode);
    spec.arguments.extend([
        format!("{MARKER_ARGUMENT}{}", marker.display()),
        format!("{SLEEP_ARGUMENT}{}", sleep.as_millis()),
    ]);
    spec
}

fn interval_job(spec: JobSpec) -> PluginUnitDefinition {
    PluginUnitDefinition::new(
        JobDefinition::new(spec).expect("valid job definition"),
        Activation::Interval {
            every: JOB_INTERVAL,
            accuracy: JOB_ACCURACY,
        },
    )
    .expect("valid plugin job")
}

fn restarting_service(mut spec: JobSpec, delay: Duration) -> PluginUnitDefinition {
    spec.restart = RestartPolicy::Always { delay };
    PluginUnitDefinition::new(
        JobDefinition::new(spec).expect("valid service definition"),
        Activation::Service,
    )
    .expect("valid plugin service")
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_plugin_job_timer_fires_repeatedly() {
    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "poll")
        .expect("valid plugin unit key");
    let (service, timer) = plugin.track(&key);
    let marker = plugin.marker("poll");
    let definition = interval_job(plugin_job(plugin.root(), "job", &marker, Duration::ZERO));

    plugin
        .units
        .install(&key, &definition)
        .await
        .expect("install plugin job");
    let marks = wait_for_marks(&marker, |marks| count(marks, false) >= REPEATED_RUNS).await;
    let pids: std::collections::BTreeSet<u32> = marks.iter().map(|mark| mark.pid).collect();
    assert!(pids.len() >= REPEATED_RUNS, "every run is its own process");
    assert_eq!(
        plugin.units.state(&key).await.expect("job state"),
        ServiceState::Running,
        "an armed schedule whose runs succeed is running"
    );

    analyze_verify(&plugin.unit_dir.join(&service));
    analyze_verify(&plugin.unit_dir.join(&timer));
    assert_eq!(unit_file_state(&timer), "enabled");
    assert_eq!(
        unit_file_state(&service),
        "static",
        "only the timer starts a job"
    );
    let properties = show(
        &service,
        &[
            "Type",
            "KillMode",
            "Restart",
            "TimeoutStartUSec",
            "StartLimitIntervalUSec",
            "Environment",
        ],
    );
    let expected = [
        ("Type", "oneshot".to_owned()),
        ("KillMode", "mixed".to_owned()),
        ("Restart", "no".to_owned()),
        ("TimeoutStartUSec", "1min".to_owned()),
        ("StartLimitIntervalUSec", "0".to_owned()),
        (
            "Environment",
            format!("XDG_STATE_HOME={}", plugin.root().join("state").display()),
        ),
    ];
    for (key, value) in expected {
        assert_eq!(properties.get(key), Some(&value), "service property {key}");
    }
    assert_eq!(
        show(&timer, &["AccuracyUSec"])
            .get("AccuracyUSec")
            .map(String::as_str),
        Some("1s")
    );

    plugin.units.remove(&key).await.expect("remove plugin job");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn an_active_plugin_job_is_never_started_twice() {
    /// Each run outlasts several intervals, so the timer elapses while it is
    /// still active.
    const RUN_LENGTH: Duration = Duration::from_secs(3);
    /// Completed runs whose windows are compared.
    const RUNS: usize = 3;

    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "slow")
        .expect("valid plugin unit key");
    plugin.track(&key);
    let marker = plugin.marker("slow");
    let definition = interval_job(plugin_job(plugin.root(), "job", &marker, RUN_LENGTH));

    plugin
        .units
        .install(&key, &definition)
        .await
        .expect("install plugin job");
    let marks = wait_for_marks(&marker, |marks| count(marks, false) >= RUNS).await;

    // Pair every run's start with its end, then require the runs to follow
    // each other: no run starts before the previous one ended.
    let mut windows: Vec<(u128, Option<u128>)> = marks
        .iter()
        .filter(|mark| mark.start)
        .map(|start| {
            let end = marks
                .iter()
                .find(|mark| !mark.start && mark.pid == start.pid)
                .map(|end| end.at);
            (start.at, end)
        })
        .collect();
    windows.sort_unstable();
    for pair in windows.windows(2) {
        let ((_, previous_end), (next_start, _)) = (pair[0], pair[1]);
        let previous_end = previous_end.expect("only the last run may still be active");
        assert!(next_start >= previous_end, "runs overlap: {windows:?}");
    }
    let ended = windows.iter().filter(|(_, end)| end.is_some()).count();
    assert!(ended >= RUNS, "{windows:?}");

    plugin.units.remove(&key).await.expect("remove plugin job");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_crashing_plugin_service_is_restarted() {
    /// Pacing of restarts; the shortest the contract accepts.
    const RESTART_DELAY: Duration = Duration::from_secs(1);

    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "notifier")
        .expect("valid plugin unit key");
    let (service, timer) = plugin.track(&key);
    let marker = plugin.marker("notifier");
    let definition = restarting_service(
        plugin_job(plugin.root(), "crash", &marker, Duration::ZERO),
        RESTART_DELAY,
    );

    plugin
        .units
        .install(&key, &definition)
        .await
        .expect("install plugin service");
    let marks = wait_for_marks(&marker, |marks| count(marks, true) >= REPEATED_RUNS).await;
    let pids: std::collections::BTreeSet<u32> = marks.iter().map(|mark| mark.pid).collect();
    assert!(pids.len() >= REPEATED_RUNS, "every start is a new process");
    let state = plugin.units.state(&key).await.expect("service state");
    assert_ne!(state, ServiceState::Failed, "systemd keeps restarting it");

    analyze_verify(&plugin.unit_dir.join(&service));
    assert!(
        !plugin.unit_dir.join(&timer).exists(),
        "a service has no timer"
    );
    assert_eq!(unit_file_state(&service), "enabled");
    let properties = show(
        &service,
        &[
            "Type",
            "Restart",
            "RestartUSec",
            "StartLimitIntervalUSec",
            "KillMode",
        ],
    );
    let expected = [
        ("Type", "exec"),
        ("Restart", "always"),
        ("RestartUSec", "1s"),
        ("StartLimitIntervalUSec", "0"),
        ("KillMode", "mixed"),
    ];
    for (key, value) in expected {
        assert_eq!(
            properties.get(key).map(String::as_str),
            Some(value),
            "service property {key}"
        );
    }

    plugin
        .units
        .remove(&key)
        .await
        .expect("remove plugin service");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn stopping_a_plugin_unit_signals_its_main_process_and_kills_the_rest() {
    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "parent")
        .expect("valid plugin unit key");
    plugin.track(&key);
    let marker = plugin.marker("parent");
    let spec = plugin_job(
        plugin.root(),
        "serve-with-descendant",
        &marker,
        Duration::ZERO,
    );
    plugin
        .units
        .install(&key, &restarting_service(spec, JOB_INTERVAL))
        .await
        .expect("install plugin service");
    let main = wait_for_marks(&marker, |marks| count(marks, true) >= 1).await[0].pid;
    let record = PathBuf::from(format!("{}.descendant", marker.display()));
    let descendant: u32 = wait::wait_until("the descendant to be recorded", || async {
        std::fs::read_to_string(&record).ok()?.parse().ok()
    })
    .await;

    plugin.units.stop(&key).await.expect("stop plugin service");
    for pid in [main, descendant] {
        assert!(
            std::fs::metadata(format!("/proc/{pid}")).is_err(),
            "process {pid} outlived the stop"
        );
    }
    assert!(
        !PathBuf::from(format!("{}.term", marker.display())).exists(),
        "the stop's SIGTERM reached a process other than the main one"
    );
    assert_eq!(
        plugin.units.state(&key).await.expect("service state"),
        ServiceState::Stopped
    );

    plugin
        .units
        .remove(&key)
        .await
        .expect("remove plugin service");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_plugin_job_run_is_stopped_at_its_timeout() {
    /// Bound on one run; the fixture would otherwise sleep for minutes.
    const RUN_TIMEOUT: Duration = Duration::from_secs(2);

    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "wedged")
        .expect("valid plugin unit key");
    let (service, _) = plugin.track(&key);
    let marker = plugin.marker("wedged");
    let mut spec = plugin_job(plugin.root(), "serve", &marker, Duration::ZERO);
    spec.start_timeout = RUN_TIMEOUT;
    spec.exit_timeout = Duration::from_secs(1);

    plugin
        .units
        .install(&key, &interval_job(spec))
        .await
        .expect("install plugin job");
    let first = wait_for_marks(&marker, |marks| count(marks, true) >= 1).await[0];
    // The result is read together with the failed state, before the next
    // run resets it one interval later.
    let units = &plugin.units;
    let key = &key;
    let service = &service;
    let result = wait::wait_until("the timed-out run's failure", move || async move {
        (units.state(key).await.expect("job state") == ServiceState::Failed)
            .then(|| show(service, &["Result"]).remove("Result"))
            .flatten()
    })
    .await;
    assert_eq!(
        result, "timeout",
        "the manager stopped the run at its bound"
    );
    assert!(
        std::fs::metadata(format!("/proc/{}", first.pid)).is_err(),
        "the timed-out run outlived its bound"
    );

    plugin.units.remove(key).await.expect("remove plugin job");
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn removed_plugin_units_leave_nothing_and_workers_never_see_them() {
    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    // A unit named `worker` sits closest to the worker glob.
    let job = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "worker")
        .expect("valid plugin unit key");
    let service = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "notifier")
        .expect("valid plugin unit key");
    let (job_service, job_timer) = plugin.track(&job);
    let (service_unit, _) = plugin.track(&service);
    let job_marker = plugin.marker("worker");
    let service_marker = plugin.marker("notifier");
    plugin
        .units
        .install(
            &job,
            &interval_job(plugin_job(
                plugin.root(),
                "job",
                &job_marker,
                Duration::ZERO,
            )),
        )
        .await
        .expect("install plugin job");
    plugin
        .units
        .install(
            &service,
            &restarting_service(
                plugin_job(plugin.root(), "serve", &service_marker, Duration::ZERO),
                Duration::from_secs(1),
            ),
        )
        .await
        .expect("install plugin service");
    wait_for_marks(&job_marker, |marks| count(marks, false) >= 1).await;
    wait_for_marks(&service_marker, |marks| count(marks, true) >= 1).await;
    plugin
        .wait_for_state(&service, |state| state == ServiceState::Running)
        .await;

    // Worker discovery of the same namespace neither adopts nor rejects them.
    let supervisor = plugin.installation.supervisor().await;
    let discovery = supervisor.discover_units().await.expect("discover workers");
    assert!(discovery.observations.is_empty(), "{discovery:?}");
    assert!(discovery.rejected_units.is_empty(), "{discovery:?}");
    assert!(!discovery.incomplete);
    assert!(supervisor
        .discover_strict()
        .await
        .expect("strict discovery")
        .is_empty());

    for key in [&job, &service] {
        plugin.units.stop(key).await.expect("stop plugin unit");
        assert_eq!(
            plugin.units.state(key).await.expect("stopped state"),
            ServiceState::Stopped,
            "{key}"
        );
        plugin.units.start(key).await.expect("start plugin unit");
        plugin
            .wait_for_state(key, |state| state == ServiceState::Running)
            .await;
    }
    assert_eq!(
        show(&service_unit, &["ActiveState"])["ActiveState"],
        "active"
    );

    for key in [&job, &service] {
        plugin.units.remove(key).await.expect("remove plugin unit");
    }
    assert!(plugin.plugin_unit_files().is_empty());
    for unit in [&job_service, &job_timer, &service_unit] {
        assert_ne!(unit_file_state(unit), "enabled", "{unit}");
    }
    wait::wait_until("no loaded plugin unit", || {
        let gone = plugin.loaded_plugin_units().is_empty();
        async move { gone.then_some(()) }
    })
    .await;
    for key in [&job, &service] {
        assert!(matches!(
            plugin.units.state(key).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            plugin.units.start(key).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            plugin.units.stop(key).await,
            Err(Error::NotFound(_))
        ));
        plugin
            .units
            .remove(key)
            .await
            .expect("removal is idempotent");
    }
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn plugin_unit_names_stay_bounded_and_safe() {
    /// Longest unit name a plugin unit key accepts.
    const LONGEST_NAME: usize = 64;
    /// Longest native unit name; the service ID bound.
    const LONGEST_UNIT: usize = 128;

    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    // The plugin ID carries every character a unit name must never hold; it
    // reaches systemd only as its hash.
    let hostile = "acme/../$HOME %h;\"q\"\\ \u{17e}\n.@";
    let plugin_id = hostile.repeat(4_096 / hostile.len());
    assert!(plugin_id.len() <= 4_096);
    let name: String = std::iter::once('j')
        .chain("_9".chars().cycle().take(LONGEST_NAME - 1))
        .collect();
    assert_eq!(name.len(), LONGEST_NAME);
    let key = PluginUnitKey::new(&plugin_id, PluginUnitKind::Job, name.clone())
        .expect("longest valid key");
    let (service, timer) = plugin.track(&key);
    let marker = plugin.marker("longest");

    for (plugin_id, name) in [
        ("acme.work", format!("{name}x")),
        ("acme.work", "Upper".to_owned()),
        ("acme.work", "has.dot".to_owned()),
        ("acme.work", "has-dash".to_owned()),
        ("acme.work", "a/b".to_owned()),
        ("acme.work", "a@b".to_owned()),
        ("acme.work", "%h".to_owned()),
        ("acme.work", "9lead".to_owned()),
        ("acme.work", "_lead".to_owned()),
        ("acme.work", String::new()),
        ("", "poll".to_owned()),
        (&"x".repeat(4_097), "poll".to_owned()),
    ] {
        assert!(
            matches!(
                PluginUnitKey::new(plugin_id, PluginUnitKind::Job, name.clone()),
                Err(Error::InvalidServiceId(_))
            ),
            "accepted {name:?}"
        );
    }

    assert!(
        plugin.plugin_unit_files().is_empty(),
        "a rejected key writes nothing"
    );

    plugin
        .units
        .install(
            &key,
            &interval_job(plugin_job(plugin.root(), "job", &marker, Duration::ZERO)),
        )
        .await
        .expect("install the longest plugin unit");
    for unit in [&service, &timer] {
        assert!(unit.len() <= LONGEST_UNIT, "{unit}");
        assert!(
            unit.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || b"-_.".contains(&byte)),
            "{unit}"
        );
        ServiceId::parse(unit.as_str()).expect("unit names are valid service IDs");
        analyze_verify(&plugin.unit_dir.join(unit));
    }
    let mut expected = vec![service.clone(), timer.clone()];
    expected.sort();
    assert_eq!(plugin.plugin_unit_files(), expected);
    wait_for_marks(&marker, |marks| count(marks, false) >= 1).await;
    assert_eq!(
        plugin.units.state(&key).await.expect("job state"),
        ServiceState::Running
    );

    plugin.units.remove(&key).await.expect("remove plugin job");
    assert!(plugin.plugin_unit_files().is_empty());
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn plugin_unit_durations_and_kinds_are_refused_before_install() {
    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "poll").expect("valid key");
    plugin.track(&key);
    let marker = plugin.marker("poll");

    // Both backends express plugin unit durations only in whole seconds of
    // at least one second.
    let job_spec = || plugin_job(plugin.root(), "job", &marker, Duration::ZERO);
    for (every, accuracy) in [
        (Duration::from_millis(1_500), JOB_ACCURACY),
        (Duration::from_millis(500), Duration::from_millis(500)),
        (Duration::from_nanos(1), Duration::from_nanos(1)),
        (JOB_INTERVAL, Duration::from_millis(100)),
        (JOB_INTERVAL, Duration::from_secs(2)),
    ] {
        assert!(
            matches!(
                PluginUnitDefinition::new(
                    JobDefinition::new(job_spec()).expect("valid job"),
                    Activation::Interval { every, accuracy },
                ),
                Err(Error::InvalidDefinition { .. })
            ),
            "accepted every {every:?} accuracy {accuracy:?}"
        );
    }
    for delay in [Duration::from_millis(200), Duration::from_millis(1_500)] {
        let mut spec = job_spec();
        spec.restart = RestartPolicy::Always { delay };
        assert!(
            matches!(
                PluginUnitDefinition::new(
                    JobDefinition::new(spec).expect("valid job"),
                    Activation::Service,
                ),
                Err(Error::InvalidDefinition { .. })
            ),
            "accepted restart delay {delay:?}"
        );
    }
    // A definition of the other kind never installs under a job key.
    let other_kind = restarting_service(job_spec(), Duration::from_secs(1));
    assert!(matches!(
        plugin.units.install(&key, &other_kind).await,
        Err(Error::InvalidDefinition { .. })
    ));
    assert!(
        plugin.plugin_unit_files().is_empty(),
        "a refused definition writes nothing"
    );
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn a_job_and_a_service_of_one_name_coexist() {
    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let job = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "sync").expect("valid key");
    let service =
        PluginUnitKey::new("acme.work", PluginUnitKind::Service, "sync").expect("valid key");
    let (job_service, job_timer) = plugin.track(&job);
    let (service_unit, _) = plugin.track(&service);
    let job_marker = plugin.marker("sync-job");
    let replaced_marker = plugin.marker("sync-job-replaced");
    let service_marker = plugin.marker("sync-service");

    plugin
        .units
        .install(
            &job,
            &interval_job(plugin_job(
                plugin.root(),
                "job",
                &job_marker,
                Duration::ZERO,
            )),
        )
        .await
        .expect("install plugin job");
    plugin
        .units
        .install(
            &service,
            &restarting_service(
                plugin_job(plugin.root(), "serve", &service_marker, Duration::ZERO),
                Duration::from_secs(1),
            ),
        )
        .await
        .expect("install plugin service of the same name");
    wait_for_marks(&job_marker, |marks| count(marks, false) >= 1).await;
    let serving = wait_for_marks(&service_marker, |marks| count(marks, true) >= 1).await[0];
    let mut expected = vec![job_service.clone(), job_timer.clone(), service_unit.clone()];
    expected.sort();
    assert_eq!(plugin.plugin_unit_files(), expected);

    // Replacing the job's definition takes effect and leaves the service
    // process alone.
    plugin
        .units
        .install(
            &job,
            &interval_job(plugin_job(
                plugin.root(),
                "job",
                &replaced_marker,
                Duration::ZERO,
            )),
        )
        .await
        .expect("replace the plugin job");
    wait_for_marks(&replaced_marker, |marks| count(marks, false) >= 1).await;
    assert!(
        std::fs::metadata(format!("/proc/{}", serving.pid)).is_ok(),
        "replacing the job stopped the service"
    );

    // Removing the service leaves the job's schedule running.
    plugin
        .units
        .remove(&service)
        .await
        .expect("remove plugin service");
    let runs = count(&read_marks(&replaced_marker), false);
    wait_for_marks(&replaced_marker, |marks| count(marks, false) > runs).await;
    let mut expected = vec![job_service, job_timer];
    expected.sort();
    assert_eq!(plugin.plugin_unit_files(), expected);
    assert!(matches!(
        plugin.units.state(&service).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        plugin.units.state(&job).await.expect("job state"),
        ServiceState::Running
    );

    plugin.units.remove(&job).await.expect("remove plugin job");
    assert!(plugin.plugin_unit_files().is_empty());
}

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn dollar_signs_reach_a_plugin_unit_verbatim() {
    require_e2e();
    let mut plugin = PluginInstallation::new().await;
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "dollar").expect("valid key");
    plugin.track(&key);
    // systemd never substitutes `$` in the program path but does in every
    // argv word, argv[0] included, unless the command opts out; `%` is a
    // specifier everywhere. All of them must reach the process unchanged.
    let directory = plugin.root().join("bin$HOME${HOME}$$%h");
    std::fs::create_dir(&directory).expect("dollar-bearing directory");
    let executable = directory.join("fixture");
    std::fs::copy(
        std::env::current_exe().expect("test executable"),
        &executable,
    )
    .expect("copy fixture executable");
    let marker = plugin.root().join("marks-$HOME-${HOME}-$$-%h");
    let mut spec = plugin_job(plugin.root(), "job", &marker, Duration::ZERO);
    spec.executable = executable.clone();

    plugin
        .units
        .install(&key, &interval_job(spec))
        .await
        .expect("install plugin job");
    wait_for_marks(&marker, |marks| count(marks, false) >= 1).await;
    assert_eq!(
        std::fs::read_to_string(argv0_record(&marker)).expect("recorded argv[0]"),
        executable.to_str().expect("UTF-8 executable"),
        "argv[0] is the program path, unexpanded"
    );

    plugin.units.remove(&key).await.expect("remove plugin job");
}

/// Removes the unit and drop-in directory a test wrote and reloads the manager.
struct UnitFiles {
    directory: PathBuf,
    unit: String,
}

impl Drop for UnitFiles {
    fn drop(&mut self) {
        systemctl(&["stop", &self.unit]);
        systemctl(&["disable", &self.unit]);
        let _ = std::fs::remove_dir_all(self.directory.join(format!("{}.d", self.unit)));
        let _ = std::fs::remove_file(self.directory.join(&self.unit));
        systemctl(&["daemon-reload"]);
    }
}

/// Removes a drop-in directory a test wrote.
struct DropInDirectory(PathBuf);

impl Drop for DropInDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        systemctl(&["daemon-reload"]);
    }
}

/// systemd applies the drop-ins of every directory sorted by file name, not
/// directory by directory, and it reads only the `[Service]` section spelled
/// exactly so: the rules the CLI's reader of drop-in files follows.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_orders_drop_ins_by_file_name_across_directories() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2e-ord-{}-x.service", std::process::id());
    let prefix_dir = directory.join(format!("pohunek-e2e-ord-{}-.service.d", std::process::id()));
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    let _prefix = DropInDirectory(prefix_dir.clone());
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\n",
    )
    .expect("unit file");
    let own = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&own).expect("own drop-in directory");
    std::fs::create_dir_all(&prefix_dir).expect("prefix drop-in directory");
    let path_of = |unit: &str| {
        let loaded = async_io::block_on(unit_environment(unit, CALL_TIMEOUT)).expect("loaded unit");
        let paths: Vec<String> = loaded
            .environment
            .iter()
            .filter(|entry| entry.starts_with("PATH="))
            .cloned()
            .collect();
        (paths, loaded.environment)
    };
    // The file name decides, whichever directory holds the file: the last
    // `PATH=` of the sorted names wins, and the merged property keeps that one.
    // Each directory order (own first, prefix first) gives a different pair.
    for (own_name, prefix_name, expected) in [
        ("90-p.conf", "10-p.conf", "PATH=/own/bin"),
        ("10-p.conf", "90-p.conf", "PATH=/prefix/bin"),
    ] {
        let _ = std::fs::remove_dir_all(&own);
        let _ = std::fs::remove_dir_all(&prefix_dir);
        std::fs::create_dir_all(&own).expect("own drop-in directory");
        std::fs::create_dir_all(&prefix_dir).expect("prefix drop-in directory");
        std::fs::write(own.join(own_name), "[Service]\nEnvironment=PATH=/own/bin\n")
            .expect("own drop-in");
        std::fs::write(
            prefix_dir.join(prefix_name),
            "[Service]\nEnvironment=PATH=/prefix/bin\n",
        )
        .expect("prefix drop-in");
        systemctl(&["daemon-reload"]);
        let (paths, environment) = path_of(&unit);
        assert_eq!(
            paths,
            [expected],
            "{own_name} / {prefix_name}: {environment:?}"
        );
    }

    // A drop-in masked by a link to `/dev/null` stays in the manager's list.
    std::os::unix::fs::symlink("/dev/null", own.join("97-mask.conf")).expect("mask");
    systemctl(&["daemon-reload"]);
    let masked = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded unit");
    assert!(
        masked
            .drop_in_paths
            .iter()
            .any(|path| path.ends_with("97-mask.conf")),
        "{:?}",
        masked.drop_in_paths
    );

    // A `[service]` section is not the `[Service]` section.
    std::fs::write(
        own.join("95-case.conf"),
        "[service]\nEnvironment=LOWERCASE=1\n",
    )
    .expect("lowercase section drop-in");
    systemctl(&["daemon-reload"]);
    let (_, environment) = path_of(&unit);
    assert!(
        !environment.iter().any(|entry| entry == "LOWERCASE=1"),
        "a [service] section is not read: {environment:?}"
    );
}

/// A drop-in file name present in the `<unit>.d` directories of two lookup
/// directories is read from the first one the manager's search path lists, and a
/// first holder linked to `/dev/null` disables the name for both: the priority
/// rule the CLI's resolver of drop-in files follows.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_reads_a_drop_in_name_from_the_first_lookup_directory() {
    require_e2e();
    let runtime = user_unit_dir();
    std::fs::create_dir_all(&runtime).expect("user unit directory");
    let search = Command::new("systemctl")
        .args(["--user", "show", "--property=UnitPath", "--value"])
        .output()
        .expect("run systemctl show");
    let search = String::from_utf8(search.stdout).expect("UTF-8 search path");
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let first: PathBuf = search
        .split_whitespace()
        .map(PathBuf::from)
        .find(|directory| directory.starts_with(&home) && directory != &runtime)
        .expect("a lookup directory under the home directory");
    let unit = format!("pohunek-e2e-prio-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: runtime.clone(),
        unit: unit.clone(),
    };
    std::fs::write(runtime.join(&unit), "[Service]\nExecStart=/bin/sleep 300\n")
        .expect("unit file");
    let runtime_dir = runtime.join(format!("{unit}.d"));
    let first_dir = first.join(format!("{unit}.d"));
    std::fs::create_dir_all(&runtime_dir).expect("runtime drop-in directory");
    std::fs::create_dir_all(&first_dir).expect("first drop-in directory");
    let _first = DropInDirectory(first_dir.clone());
    std::fs::write(
        runtime_dir.join("50-p.conf"),
        "[Service]\nEnvironment=PICK=runtime\n",
    )
    .expect("runtime drop-in");
    std::fs::write(
        first_dir.join("50-p.conf"),
        "[Service]\nEnvironment=PICK=first\n",
    )
    .expect("first drop-in");
    systemctl(&["daemon-reload"]);
    let loaded = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded unit");
    let listed: Vec<&String> = loaded
        .drop_ins_on_disk
        .iter()
        .filter(|path| path.ends_with("/50-p.conf"))
        .collect();
    assert_eq!(listed.len(), 2, "{loaded:?}");
    assert_eq!(
        loaded.drop_in_paths,
        [listed[0].clone()],
        "the first listed holder of the name is the loaded one: {loaded:?}"
    );
    let pick = |environment: &[String]| {
        environment
            .iter()
            .filter(|entry| entry.starts_with("PICK="))
            .cloned()
            .collect::<Vec<_>>()
    };
    let expected = if listed[0].starts_with(first.to_str().expect("UTF-8 path")) {
        "PICK=first"
    } else {
        "PICK=runtime"
    };
    assert_eq!(pick(&loaded.environment), [expected], "{loaded:?}");

    // A first holder masked by a link to `/dev/null` disables the name.
    let winner = PathBuf::from(listed[0]);
    std::fs::remove_file(&winner).expect("remove winner");
    std::os::unix::fs::symlink("/dev/null", &winner).expect("mask");
    systemctl(&["daemon-reload"]);
    let masked = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded unit");
    assert!(
        pick(&masked.environment).is_empty(),
        "a masked first holder disables the name for the later directory: {masked:?}"
    );
}

/// The reader rules the CLI's resolver of drop-in files follows: systemd skips a
/// hidden drop-in, ignores a relative `EnvironmentFile=`, and a backslash
/// followed by white space is not a continuation.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_skips_hidden_drop_ins_and_ignores_relative_environment_files() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2e-edge-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\n",
    )
    .expect("unit file");
    let own = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&own).expect("drop-in directory");
    std::fs::write(
        own.join(".hidden.conf"),
        "[Service]\nEnvironment=HIDDEN=1\n",
    )
    .expect("hidden");
    std::fs::write(
        own.join("10-rel.conf"),
        "[Service]\nEnvironmentFile=relative.env\nEnvironmentFile=-also-relative.env\n",
    )
    .expect("relative");
    std::fs::write(
        own.join("20-cont.conf"),
        "[Service]\nEnvironment=FIRST=1 \\ \nEnvironment=SECOND=2\nEnvironment=THIRD=3 \\\t\nEnvironment=FOURTH=4\n",
    )
    .expect("continuation");
    systemctl(&["daemon-reload"]);

    let loaded = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded unit");
    let listed = |paths: &[String], name: &str| paths.iter().any(|path| path.ends_with(name));
    assert!(
        !listed(&loaded.drop_in_paths, ".hidden.conf")
            && !loaded.environment.iter().any(|entry| entry == "HIDDEN=1"),
        "the manager skips a hidden drop-in: {loaded:?}"
    );
    assert!(
        !listed(&loaded.drop_ins_on_disk, ".hidden.conf"),
        "the on-disk listing skips it too: {:?}",
        loaded.drop_ins_on_disk
    );
    assert!(listed(&loaded.drop_ins_on_disk, "10-rel.conf"));
    assert!(
        loaded.environment_files.is_empty(),
        "a relative EnvironmentFile= is ignored: {:?}",
        loaded.environment_files
    );
    for name in ["FIRST=1", "SECOND=2", "THIRD=3", "FOURTH=4"] {
        assert!(
            loaded.environment.iter().any(|entry| entry == name),
            "{name}: a backslash then white space is not a continuation: {:?}",
            loaded.environment
        );
    }
}

/// The `EnvironmentFile=` reader rules the CLI's scan follows: a `PATH=` text
/// inside a quoted or continued value or a comment is not an assignment, and a
/// carriage return ends a line like a line feed.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_reads_environment_files_as_the_cli_scan_does() {
    require_e2e();
    let directory = pohunek_test_support::tempdir().expect("scratch directory");
    let assigned = |content: &str| -> bool {
        let file = directory.path().join("job.env");
        let output = directory.path().join("job.out");
        std::fs::write(&file, content).expect("environment file");
        let _ = std::fs::remove_file(&output);
        let status = Command::new("systemd-run")
            .args(["--user", "--wait", "--collect", "--quiet"])
            .arg(format!("--property=EnvironmentFile={}", file.display()))
            .arg(format!(
                "--property=StandardOutput=file:{}",
                output.display()
            ))
            .args(["/usr/bin/env", "-0"])
            .status()
            .expect("run systemd-run");
        assert!(status.success(), "systemd-run failed for {content:?}");
        std::fs::read_to_string(&output)
            .expect("job output")
            .split('\0')
            .any(|entry| entry == "PATH=/pinned/bin")
    };
    for content in [
        "OTHER='first\nPATH=/pinned/bin\nlast'\n",
        "OTHER=\"a\nPATH=/pinned/bin\"\nSTORE=/s\n",
        "OTHER=a\\\nPATH=/pinned/bin\nSTORE=/s\n",
        "# PATH=/pinned/bin\n; PATH=/pinned/bin\nSTORE=/s\n",
    ] {
        assert!(!assigned(content), "not an assignment of PATH: {content:?}");
    }
    for content in [
        "OTHER=\"a b\" c\nPATH=/pinned/bin\n",
        "OTHER=it's \"x\nPATH=/pinned/bin\n",
        "PATH=/pinned/bin\n",
        "FOO=1\rPATH=/pinned/bin\n",
        "# note\rPATH=/pinned/bin\n",
        "FOO=1\r\nPATH=/pinned/bin\r\n",
    ] {
        assert!(assigned(content), "assigns PATH: {content:?}");
    }
}

/// Enabling a unit that has a drop-in, after the reload that loaded it, does not
/// leave the unit with a stale snapshot: the gates that refuse on
/// `NeedDaemonReload` do not trip on an installed daemon unit.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn enabling_a_loaded_unit_with_a_drop_in_leaves_no_pending_reload() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2e-enable-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\n[Install]\nWantedBy=default.target\n",
    )
    .expect("unit file");
    let drop_ins = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&drop_ins).expect("drop-in directory");
    std::fs::write(
        drop_ins.join("path.conf"),
        "[Service]\nEnvironment=PATH=/drop-in/bin\n",
    )
    .expect("drop-in");
    systemctl(&["daemon-reload"]);
    systemctl(&["enable", &unit]);

    let enabled = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("enabled unit");
    assert_eq!(enabled.load_state, "loaded");
    assert!(
        !enabled.need_daemon_reload,
        "enabling must not leave a stale snapshot: {enabled:?}"
    );
    assert!(!enabled.drop_in_paths.is_empty(), "{enabled:?}");
}

/// The real user manager reports the environment a unit and its drop-ins give a
/// job: the merged `Environment=`, the `EnvironmentFile=`, `PassEnvironment=` and
/// `UnsetEnvironment=` settings, and a drop-in on a unit with no file.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_reports_a_units_environment_settings_with_drop_ins() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2e-env-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };

    // No file and no drop-in: not found, nothing configured.
    let absent = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("absent unit");
    assert_eq!(absent.load_state, "not-found");
    assert!(absent.drop_in_paths.is_empty() && absent.environment.is_empty());

    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\nEnvironment=PATH=/unit/bin\nEnvironmentFile=-/nonexistent/pohunek-e2e.env\nPassEnvironment=HOME\nUnsetEnvironment=GONE\n",
    )
    .expect("unit file");
    let drop_ins = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&drop_ins).expect("drop-in directory");
    std::fs::write(
        drop_ins.join("path.conf"),
        "[Service]\nEnvironment=PATH=/drop-in/bin\n",
    )
    .expect("drop-in");
    systemctl(&["daemon-reload"]);

    let loaded = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded unit");
    assert_eq!(loaded.load_state, "loaded");
    assert!(!loaded.need_daemon_reload);
    assert!(
        loaded
            .drop_ins_on_disk
            .iter()
            .any(|path| path.ends_with("path.conf")),
        "{:?}",
        loaded.drop_ins_on_disk
    );
    assert!(
        loaded.fragment_path.ends_with(&unit),
        "{:?}",
        loaded.fragment_path
    );
    assert_eq!(
        loaded.environment.last().map(String::as_str),
        Some("PATH=/drop-in/bin")
    );
    assert!(
        loaded
            .drop_in_paths
            .iter()
            .any(|path| path.ends_with("path.conf")),
        "{:?}",
        loaded.drop_in_paths
    );
    assert_eq!(
        loaded.environment_files,
        [("/nonexistent/pohunek-e2e.env".to_owned(), true)]
    );
    assert_eq!(loaded.pass_environment, ["HOME"]);
    assert_eq!(loaded.unset_environment, ["GONE"]);

    // A drop-in edited while the unit stays loaded (it runs) leaves the
    // properties stale.
    systemctl(&["start", &unit]);
    std::fs::write(
        drop_ins.join("path.conf"),
        "[Service]\nEnvironment=PATH=/edited/bin\n",
    )
    .expect("edited drop-in");
    let stale = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("stale unit");
    assert!(stale.need_daemon_reload, "{stale:?}");
    assert!(!stale.drop_ins_on_disk.is_empty(), "{stale:?}");
}

/// What systemd itself does with the forms the resolver has to agree with: an
/// orphan drop-in, a dash-truncated drop-in directory, `ExecSearchPath=`, a byte
/// order mark, CR line ends and a word with an unknown specifier.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_agrees_with_the_unit_setting_readers() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let id = std::process::id();
    let unit = format!("pohunek-e2eform-{id}.service");
    let truncated = "pohunek-e2eform-.service.d".to_owned();
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    let _truncated = UnitFiles {
        directory: directory.clone(),
        unit: truncated.trim_end_matches(".d").to_owned(),
    };

    // An orphan drop-in: the manager reports no file and no drop-in, the scan
    // of the manager's own lookup directories finds it.
    let own = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&own).expect("drop-in directory");
    std::fs::write(
        own.join("orphan.conf"),
        "[Service]\nEnvironment=PATH=/orphan/bin\n",
    )
    .expect("orphan drop-in");
    systemctl(&["daemon-reload"]);
    let orphan = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("orphan");
    assert_eq!(orphan.load_state, "not-found");
    assert!(orphan.drop_in_paths.is_empty(), "{orphan:?}");
    assert!(
        orphan
            .drop_ins_on_disk
            .iter()
            .any(|path| path.ends_with("orphan.conf")),
        "{orphan:?}"
    );

    // Now with a unit file: a BOM, CR line ends, a dash-truncated directory,
    // ExecSearchPath= and a dropped word.
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\nExecSearchPath=/unit-file/bin\n",
    )
    .expect("unit");
    std::fs::remove_file(own.join("orphan.conf")).expect("remove orphan");
    std::fs::write(
        own.join("bom.conf"),
        b"\xef\xbb\xbf[Service]\nEnvironment=PATH=/bom/bin\nExecSearchPath=/opt/tools/bin\n",
    )
    .expect("bom drop-in");
    std::fs::write(own.join("cr.conf"), b"[Service]\rEnvironment=CRVAR=1\r").expect("cr drop-in");
    // CRLF is one line end, so the continuation joins the next line.
    std::fs::write(
        own.join("crlf.conf"),
        b"[Service]\r\nEnvironment=CRLF1=1 \\\r\nCRLF2=2\r\n",
    )
    .expect("crlf drop-in");
    let shared = directory.join(&truncated);
    std::fs::create_dir_all(&shared).expect("truncated drop-in directory");
    std::fs::write(
        shared.join("dropped.conf"),
        "[Service]\nEnvironment=PATH=%Z/bin DROPPED=1 KEPT=1\n",
    )
    .expect("dropped drop-in");
    systemctl(&["daemon-reload"]);
    let loaded = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded");
    assert_eq!(loaded.load_state, "loaded");
    assert!(
        loaded.environment.contains(&"PATH=/bom/bin".to_owned()),
        "{loaded:?}"
    );
    assert!(
        loaded.environment.contains(&"CRVAR=1".to_owned()),
        "{loaded:?}"
    );
    // The directive appends across files: the unit file's entry stays in the
    // merged list next to the drop-in's.
    for entry in ["CRLF1=1", "CRLF2=2"] {
        assert!(loaded.environment.contains(&entry.to_owned()), "{loaded:?}");
    }
    assert_eq!(
        loaded.exec_search_path,
        ["/unit-file/bin", "/opt/tools/bin"]
    );
    // The word with the unknown specifier is dropped, its neighbours are kept.
    assert!(
        !loaded.environment.iter().any(|entry| entry.contains("%Z")),
        "{loaded:?}"
    );
    assert!(
        loaded.environment.contains(&"KEPT=1".to_owned()),
        "{loaded:?}"
    );
    for name in ["bom.conf", "cr.conf", "dropped.conf"] {
        assert!(
            loaded.drop_in_paths.iter().any(|path| path.ends_with(name)),
            "{name}: {:?}",
            loaded.drop_in_paths
        );
        assert!(
            loaded
                .drop_ins_on_disk
                .iter()
                .any(|path| path.ends_with(name)),
            "{name}: {:?}",
            loaded.drop_ins_on_disk
        );
    }

    assert_unreadable_drop_in_directory_is_an_error(&own, &unit);
}

/// The value forms the CLI's reader of a removed unit's drop-ins follows:
/// comment lines, a NUL line end, a quoted word, a no-break space, an
/// `EnvironmentFile=` path with a space, empty entries of `ExecSearchPath=` and
/// an empty assignment resetting each list.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_agrees_on_the_value_forms_of_a_drop_in() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2eval-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\n",
    )
    .expect("unit file");
    let own = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&own).expect("drop-in directory");
    std::fs::write(
        own.join("10-a.conf"),
        "[Service]\nEnvironment=KEEP=1\nEnvironmentFile=-/nonexistent/spaced path.env\nExecSearchPath=/a/bin\n",
    )
    .expect("first drop-in");
    std::fs::write(
        own.join("20-b.conf"),
        "[Service]\n# Environment=COMMENT=1\n; Environment=SEMI=1\nEnvironment=\"QUOTED=a b\"\nEnvironment=NBSP=1\u{a0}X=2\nExecSearchPath=/b/bin::/c/bin\n",
    )
    .expect("second drop-in");
    std::fs::write(
        own.join("25-nul.conf"),
        b"[Service]\0Environment=NULVAR=1\0",
    )
    .expect("NUL drop-in");
    systemctl(&["daemon-reload"]);
    let loaded = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded");
    for entry in ["KEEP=1", "QUOTED=a b", "NBSP=1\u{a0}X=2", "NULVAR=1"] {
        assert!(
            loaded.environment.contains(&entry.to_owned()),
            "{entry}: {loaded:?}"
        );
    }
    for entry in ["COMMENT=1", "SEMI=1"] {
        assert!(
            !loaded.environment.iter().any(|value| value.contains(entry)),
            "{entry}: {loaded:?}"
        );
    }
    // An empty entry is not skipped: the entries after it are not applied, which
    // is why the resolver refuses a list holding one.
    assert_eq!(loaded.exec_search_path, ["/a/bin", "/b/bin"]);
    assert_eq!(
        loaded.environment_files,
        [("/nonexistent/spaced path.env".to_owned(), true)]
    );

    std::fs::write(
        own.join("30-reset.conf"),
        "[Service]\nEnvironment=\nEnvironmentFile=\nExecSearchPath=\nEnvironment=AFTER=1\n",
    )
    .expect("reset drop-in");
    systemctl(&["daemon-reload"]);
    let reset = async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded");
    assert_eq!(reset.environment, ["AFTER=1"], "{reset:?}");
    assert!(reset.exec_search_path.is_empty(), "{reset:?}");
    assert!(reset.environment_files.is_empty(), "{reset:?}");
}

/// What the manager does with the drop-in syntax the CLI's reader of a removed
/// unit's drop-ins refuses: quotes in `ExecSearchPath=`, entries that are not
/// normalized, a section header that is never closed and a byte order mark in
/// the middle of the file.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_reads_unusual_drop_in_syntax_as_the_cli_refuses_it() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2esyn-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\n",
    )
    .expect("unit file");
    let own = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&own).expect("drop-in directory");
    let loaded_with = |content: &[u8]| {
        std::fs::write(own.join("10-case.conf"), content).expect("drop-in");
        systemctl(&["daemon-reload"]);
        async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded")
    };
    let quoted = loaded_with(b"[Service]\nExecSearchPath=\"/q/bin\"\n");
    assert!(
        quoted.exec_search_path.is_empty(),
        "quotes are literal: {quoted:?}"
    );
    let parent = loaded_with(b"[Service]\nExecSearchPath=/d/../e:/f\n");
    assert!(
        parent.exec_search_path.is_empty(),
        "a `..` entry and those after it are dropped: {parent:?}"
    );
    let simplified = loaded_with(b"[Service]\nExecSearchPath=/g//h/:/i/./j\n");
    assert_eq!(
        simplified.exec_search_path,
        ["/g/h", "/i/j"],
        "{simplified:?}"
    );
    let bogus = loaded_with(b"[Service]\nEnvironment=PATH=/a\n[Bogus\nEnvironment=PATH=/b\n");
    assert!(
        bogus.environment.contains(&"PATH=/a".to_owned())
            && !bogus.environment.contains(&"PATH=/b".to_owned()),
        "nothing after an unclosed header is read: {bogus:?}"
    );
    let noted =
        loaded_with(b"[Service]\nExecSearchPath=/x/bin\n[Service] # note\nExecSearchPath=/y/bin\n");
    assert_eq!(noted.exec_search_path, ["/x/bin"], "{noted:?}");
    let form_feed =
        loaded_with(b"[Service]\nEnvironment=PATH=/a\n[Unit]\x0C\nEnvironment=PATH=/b\n");
    assert!(
        form_feed.environment.contains(&"PATH=/a".to_owned())
            && !form_feed.environment.contains(&"PATH=/b".to_owned()),
        "a form feed is not white space in a header line: {form_feed:?}"
    );
    let noncharacter =
        loaded_with(b"[Service]\nEnvironment=PATH=/a\nEnvironment=PATH=/b FOO=\xEF\xBF\xBE\n");
    assert!(
        noncharacter.environment.contains(&"PATH=/a".to_owned())
            && !noncharacter.environment.contains(&"PATH=/b".to_owned()),
        "a line with a noncharacter is dropped: {noncharacter:?}"
    );
    let long_component = format!(
        "[Service]\nExecSearchPath=/x:/{}:/y\nExecSearchPath=/z\n",
        "a".repeat(256)
    );
    let long = loaded_with(long_component.as_bytes());
    assert_eq!(long.exec_search_path, ["/x", "/z"], "{long:?}");
    let bom = loaded_with(b"[Service]\nEnvironment=FIRST=1\n\xEF\xBB\xBFEnvironment=PATH=/bom\n");
    assert!(
        bom.environment.contains(&"PATH=/bom".to_owned()),
        "a byte order mark mid-file is dropped: {bom:?}"
    );
}

/// The tolerated forms the CLI's reader of a removed unit's drop-ins follows as
/// systemd does: a specifier in a variable name, single and joined quotes, an
/// empty value, an empty word, space before a header or around the key, a
/// reopened `[Service]` section and an `EnvironmentFile=` path with a space after
/// the dash.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_reads_tolerated_drop_in_forms_as_the_cli_does() {
    require_e2e();
    let directory = user_unit_dir();
    std::fs::create_dir_all(&directory).expect("user unit directory");
    let unit = format!("pohunek-e2etol-{}.service", std::process::id());
    let _files = UnitFiles {
        directory: directory.clone(),
        unit: unit.clone(),
    };
    std::fs::write(
        directory.join(&unit),
        "[Service]\nExecStart=/bin/sleep 300\n",
    )
    .expect("unit file");
    let own = directory.join(format!("{unit}.d"));
    std::fs::create_dir_all(&own).expect("drop-in directory");
    let loaded_with = |content: &[u8]| {
        std::fs::write(own.join("10-case.conf"), content).expect("drop-in");
        systemctl(&["daemon-reload"]);
        async_io::block_on(unit_environment(&unit, CALL_TIMEOUT)).expect("loaded")
    };
    let has = |loaded: &pohunek_platform::supervisor::UnitEnvironment, entry: &str| {
        loaded.environment.contains(&entry.to_owned())
    };
    for (content, entry) in [
        (&b"[Service]\nEnvironment=%iPATH=/x\n"[..], "PATH=/x"),
        (b"[Service]\nEnvironment=PATH='/a'\"/b\"c\n", "PATH=/a/bc"),
        (b"[Service]\nEnvironment=PATH=\n", "PATH="),
        (b"[Service]\nEnvironment=\"\" PATH=/e\n", "PATH=/e"),
        (b"  [Service]\nEnvironment=WS=1\n", "WS=1"),
        (b"[Service]\nEnvironment =SPK=1\n", "SPK=1"),
        (
            b"[Unit]\nDescription=x\n[Service]\nEnvironment=RE=1\n[Unit]\n[Service]\nEnvironment=RE2=1\n",
            "RE2=1",
        ),
    ] {
        let loaded = loaded_with(content);
        assert!(has(&loaded, entry), "{content:?}: {loaded:?}");
    }
    let dash = loaded_with(b"[Service]\nEnvironmentFile=- /x\n");
    assert!(dash.environment_files.is_empty(), "{dash:?}");
}

/// A drop-in directory the scan cannot read is an error, never "no drop-ins".
fn assert_unreadable_drop_in_directory_is_an_error(own: &std::path::Path, unit: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(own, std::fs::Permissions::from_mode(0o000)).expect("lock");
    let unreadable = async_io::block_on(unit_environment(unit, CALL_TIMEOUT));
    std::fs::set_permissions(own, std::fs::Permissions::from_mode(0o755)).expect("unlock");
    assert!(
        matches!(unreadable, Err(Error::Operation { .. })),
        "{unreadable:?}"
    );
}

/// The real user manager reports the `KEY=VALUE` environment its units get,
/// with a `PATH` entry.
#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
fn the_real_user_manager_reports_its_environment_with_a_path() {
    require_e2e();
    let environment = async_io::block_on(user_manager_environment(CALL_TIMEOUT))
        .expect("the user manager answers the Environment property");
    assert!(
        environment.iter().any(|entry| entry.starts_with("PATH=")),
        "{environment:?}"
    );
}
