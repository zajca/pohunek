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
//! The daemon test writes into the real user manager's runtime unit directory
//! (`$XDG_RUNTIME_DIR/systemd/user`), because a user manager loads persistent
//! units only from its search path. Its unit names embed the unique
//! namespace, and the guard disables and removes them.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use pohunek_platform::supervisor::systemd::{
    render_daemon_unit, render_sessions_slice, SystemdDaemon, SystemdSupervisor,
};
use pohunek_platform::supervisor::{
    DaemonSupervisor, Error, JobDefinition, JobSpec, Namespace, RestartPolicy, ServiceId,
    ServiceObservation, ServiceState, Supervisor, WorkerKey,
};
use tempfile::TempDir;

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
/// Session ID shared by the tests; namespaces keep them apart.
const SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN2";

fn require_e2e() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "set {E2E_VARIABLE}=1 explicitly"
    );
}

/// Unit main process used by every worker test.
///
/// Modes: `ready` notifies and sleeps; `never-ready` sleeps without notifying;
/// `ready-then-fail` notifies and exits with status 3; `stubborn-descendants`
/// starts a child and a grandchild that ignore `SIGHUP` and `SIGTERM`, then
/// notifies and sleeps.
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
        other => panic!("unknown fixture mode {other}"),
    }
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

/// A namespace unique to one test and the directory it was derived from.
struct Installation {
    root: TempDir,
    namespace: Namespace,
    cleanup: Cleanup,
}

impl Installation {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary installation root");
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
    JobSpec {
        executable: std::env::current_exe().expect("test executable"),
        arguments: vec![
            "fixture_entry".to_owned(),
            "--exact".to_owned(),
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

    daemon.install(&definition).await.expect("install daemon");
    let first = wait_for_daemon(&daemon, |observation| {
        observation.state == ServiceState::Running
    })
    .await;
    assert_eq!(first.id.as_str(), daemon_unit);
    assert_eq!(first.definition, Some(definition.facts()));
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", &daemon_unit])
        .output()
        .expect("run systemctl is-enabled");
    assert_eq!(String::from_utf8_lossy(&enabled.stdout).trim(), "enabled");
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
