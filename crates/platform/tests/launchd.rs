//! Exercises the launchd backends against the real `gui/<uid>` domain.
//!
//! Every test derives its own namespace from a fresh temporary directory, so
//! labels never collide with a real installation or a parallel test, and a
//! drop guard boots out every label the test created. The daemon agent tests
//! use a temporary `LaunchAgents` directory; production passes
//! `~/Library/LaunchAgents`, which these tests never touch.
//!
//! The tests are not ignored: the macOS CI job runs them, and a missing GUI
//! domain fails them instead of skipping.
#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-09-24

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pohunek_platform::process::{HostInspector, ProcessIdentity, ProcessInspector as _};
use pohunek_platform::supervisor::launchd::{LaunchdDaemon, LaunchdSupervisor};
use pohunek_platform::supervisor::{
    DaemonSupervisor as _, Error, JobDefinition, JobSpec, Namespace, RestartPolicy, ServiceId,
    ServiceObservation, ServiceState, Supervisor as _, WorkerKey,
};

/// Deadline of one `launchctl` command; generous for loaded CI runners.
const COMMAND_DEADLINE: Duration = Duration::from_secs(30);

/// Longest wait for launchd to spawn a job's process.
const SPAWN_WAIT: Duration = Duration::from_secs(30);

/// launchd's reap margin after `ExitTimeOut`, mirrored from the backend.
const ABSENCE_MARGIN: Duration = Duration::from_secs(5);

/// Delay between observation polls.
const POLL: Duration = Duration::from_millis(100);

/// uid far outside the range macOS assigns, so `gui/<uid>` never exists.
const ABSENT_UID: u32 = 4_000_000;

/// Worker that runs until launchd stops it.
const SLEEPING_WORKER: &str = "/bin/sleep 600 & wait";

/// Worker that ignores `SIGTERM` and keeps a child in its process group.
const STUBBORN_WORKER: &str = "trap '' TERM; /bin/sleep 600 & wait";

fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn launchctl(arguments: &[&str]) -> std::process::ExitStatus {
    Command::new("/bin/launchctl")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("launchctl runs")
}

/// Fails the test unless the per-user GUI domain exists.
fn require_gui_domain() {
    let domain = format!("gui/{}", uid());
    assert!(
        launchctl(&["print", &domain]).success(),
        "launchd domain {domain} is unavailable; these tests need a logged-in GUI session"
    );
}

/// One test's namespace, directories, and cleanup.
struct Fixture {
    root: PathBuf,
    namespace: Namespace,
    definitions: PathBuf,
    logs: PathBuf,
    supervisor: LaunchdSupervisor,
    /// Labels loaded outside the supervisor, booted out on drop.
    extra_labels: Vec<String>,
    _temporary: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        require_gui_domain();
        let temporary = tempfile::tempdir().expect("temporary directory");
        // `/var` is a symlink on macOS; trusted directories never follow one.
        let root = std::fs::canonicalize(temporary.path()).expect("canonical root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private root");
        let namespace = Namespace::derive(uid(), &root.join("state"), &root.join("runtime"));
        let definitions = root.join("state/pohunek/launchd");
        let logs = root.join("logs/launchd");
        let supervisor = LaunchdSupervisor::new(
            namespace.clone(),
            uid(),
            definitions.clone(),
            logs.clone(),
            COMMAND_DEADLINE,
        )
        .expect("valid supervisor");
        Self {
            root,
            namespace,
            definitions,
            logs,
            supervisor,
            extra_labels: Vec::new(),
            _temporary: temporary,
        }
    }

    fn worker(&self, key: &WorkerKey, script: &str, exit_timeout: Duration) -> JobDefinition {
        JobDefinition::new(JobSpec {
            executable: PathBuf::from("/bin/bash"),
            arguments: vec![
                "-c".to_owned(),
                script.to_owned(),
                "pohunek-test-worker".to_owned(),
                "--session-id".to_owned(),
                key.session_id().to_owned(),
                "--worker-generation".to_owned(),
                key.generation().to_owned(),
            ],
            environment: BTreeMap::from([(
                "HOME".to_owned(),
                self.root.to_str().expect("UTF-8 root").to_owned(),
            )]),
            working_directory: self.root.clone(),
            logs: Some(self.supervisor.worker_logs(key)),
            start_timeout: Duration::from_secs(10),
            exit_timeout,
            restart: RestartPolicy::Never,
            open_files: 1_024,
        })
        .expect("valid worker definition")
    }

    fn definition_path(&self, key: &WorkerKey) -> PathBuf {
        self.definitions
            .join(format!("{}.plist", self.namespace.worker_label(key)))
    }

    /// Writes a plist into the definitions directory and optionally loads it.
    fn plant(&mut self, file_name: &str, contents: &[u8], load: Option<&str>) {
        let path = self.definitions.join(file_name);
        std::fs::write(&path, contents).expect("definition written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("definition mode set");
        if let Some(label) = load {
            self.extra_labels.push(label.to_owned());
            let domain = format!("gui/{}", uid());
            let status = launchctl(&["bootstrap", &domain, path.to_str().expect("UTF-8 path")]);
            assert!(status.success(), "bootstrap of {label} failed: {status}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut labels = self.extra_labels.clone();
        if let Ok(entries) = std::fs::read_dir(&self.definitions) {
            for entry in entries.flatten() {
                if let Some(label) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".plist"))
                {
                    labels.push(label.to_owned());
                }
            }
        }
        for label in labels {
            // Teardown only: an already absent label is the desired state.
            let _status = launchctl(&["bootout", &format!("gui/{}/{label}", uid())]);
        }
    }
}

fn key(session: &str, generation: &str) -> WorkerKey {
    WorkerKey::new(session, generation).expect("valid worker key")
}

fn plist_document(label: &str, program: &[&str]) -> Vec<u8> {
    let mut job = plist::Dictionary::new();
    job.insert("Label".to_owned(), plist::Value::String(label.to_owned()));
    job.insert(
        "ProgramArguments".to_owned(),
        plist::Value::Array(
            program
                .iter()
                .map(|value| plist::Value::String((*value).to_owned()))
                .collect(),
        ),
    );
    job.insert("RunAtLoad".to_owned(), plist::Value::Boolean(true));
    job.insert(
        "ExitTimeOut".to_owned(),
        plist::Value::Integer(plist::Integer::from(2_u64)),
    );
    let mut bytes = Vec::new();
    plist::Value::Dictionary(job)
        .to_writer_xml(&mut bytes)
        .expect("plist renders");
    bytes
}

/// Polls `inspect` until the observation satisfies `ready`.
async fn wait_for<F>(
    mut inspect: impl FnMut() -> F,
    ready: impl Fn(&ServiceObservation) -> bool,
) -> ServiceObservation
where
    F: Future<Output = Result<ServiceObservation, Error>>,
{
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        match inspect().await {
            Ok(observation) if ready(&observation) => return observation,
            Ok(_) | Err(Error::Race { .. }) => {}
            Err(error) => panic!("inspection failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "job never reached the expected state"
        );
        tokio::time::sleep(POLL).await;
    }
}

fn running(observation: &ServiceObservation) -> bool {
    observation.state == ServiceState::Running && observation.process.is_some()
}

async fn wait_gone(identities: &[ProcessIdentity], deadline: Instant) {
    let inspector = HostInspector::new();
    loop {
        let alive = identities
            .iter()
            .filter(|identity| inspector.is_running(**identity).expect("liveness"))
            .count();
        if alive == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{alive} job processes outlived retirement"
        );
        tokio::time::sleep(POLL).await;
    }
}

#[tokio::test]
async fn start_inspect_and_retire_one_worker() {
    let fixture = Fixture::new();
    let key = key("s-1001", "abcd2345");
    let id = key.service_id();
    let definition = fixture.worker(&key, SLEEPING_WORKER, Duration::from_secs(5));

    fixture
        .supervisor
        .start(&id, &definition)
        .await
        .expect("worker starts");
    let observation = wait_for(|| fixture.supervisor.inspect(&id), running).await;
    assert_eq!(observation.id, id);
    assert_eq!(observation.definition, Some(definition.facts()));
    let process = observation.process.expect("running worker has a process");
    let inspector = HostInspector::new();
    assert_eq!(inspector.parent_pid(process.pid).expect("parent"), Some(1));

    let path = fixture.definition_path(&key);
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&fixture.definitions), 0o700);
    assert_eq!(mode(&fixture.logs), 0o700);

    // A second start of a loaded generation never rewrites its definition.
    let before = std::fs::read(&path).expect("definition readable");
    let other = fixture.worker(&key, "/bin/sleep 700 & wait", Duration::from_secs(5));
    assert!(matches!(
        fixture.supervisor.start(&id, &other).await,
        Err(Error::AlreadyRegistered(registered)) if registered == id
    ));
    assert_eq!(std::fs::read(&path).expect("definition readable"), before);

    fixture
        .supervisor
        .retire(&id)
        .await
        .expect("worker retires");
    assert!(matches!(
        fixture.supervisor.inspect(&id).await,
        Err(Error::NotFound(missing)) if missing == id
    ));
    wait_gone(&[process], Instant::now() + ABSENCE_MARGIN).await;
    assert!(!path.exists(), "definition removed on retirement");
    let logs = fixture.supervisor.worker_logs(&key);
    assert!(
        !logs.stdout.exists() && !logs.stderr.exists(),
        "logs removed"
    );
    assert!(matches!(
        fixture.supervisor.retire(&id).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn discovery_ignores_foreign_other_namespace_and_malformed_definitions() {
    let mut fixture = Fixture::new();
    let own = key("s-2001", "efgh2345");
    let own_id = own.service_id();
    let definition = fixture.worker(&own, SLEEPING_WORKER, Duration::from_secs(5));
    fixture
        .supervisor
        .start(&own_id, &definition)
        .await
        .expect("worker starts");

    let foreign = format!("com.example.pohunek-test.{}", fixture.namespace);
    fixture.plant(
        &format!("{foreign}.plist"),
        &plist_document(&foreign, &["/bin/sleep", "600"]),
        Some(&foreign),
    );
    let other_namespace = Namespace::derive(uid(), &fixture.root.join("other"), &fixture.root);
    let other = other_namespace.worker_label(&key("s-2002", "ijkl2345"));
    fixture.plant(
        &format!("{other}.plist"),
        &plist_document(&other, &["/bin/sleep", "600"]),
        Some(&other),
    );
    let malformed = fixture.namespace.worker_label(&key("s-2003", "mnop2345"));
    fixture.plant(&format!("{malformed}.plist"), b"not a plist", None);
    let mismatched = fixture.namespace.worker_label(&key("s-2004", "qrst2345"));
    fixture.plant(
        &format!("{mismatched}.plist"),
        &plist_document(&foreign, &["/bin/sleep", "600"]),
        None,
    );

    wait_for(|| fixture.supervisor.inspect(&own_id), running).await;
    let discovery = fixture
        .supervisor
        .discover_definitions()
        .await
        .expect("discovery succeeds");
    let ids: Vec<&ServiceId> = discovery
        .observations
        .iter()
        .map(|observation| &observation.id)
        .collect();
    assert_eq!(ids, [&own_id]);
    let mut rejected = discovery.rejected.clone();
    rejected.sort();
    let mut expected = vec![format!("{malformed}.plist"), format!("{mismatched}.plist")];
    expected.sort();
    assert_eq!(rejected, expected);
    assert_eq!(
        fixture
            .supervisor
            .discover()
            .await
            .expect("discovery")
            .len(),
        1
    );

    // Foreign jobs stay loaded; discovery never touches them.
    let domain = format!("gui/{}", uid());
    assert!(launchctl(&["print", &format!("{domain}/{foreign}")]).success());
    assert!(launchctl(&["print", &format!("{domain}/{other}")]).success());

    fixture
        .supervisor
        .retire(&own_id)
        .await
        .expect("worker retires");
    assert!(fixture
        .supervisor
        .discover()
        .await
        .expect("discovery")
        .is_empty());
}

#[tokio::test]
async fn an_absent_domain_is_domain_unavailable() {
    let fixture = Fixture::new();
    let supervisor = LaunchdSupervisor::new(
        fixture.namespace.clone(),
        ABSENT_UID,
        fixture.definitions.clone(),
        fixture.logs.clone(),
        COMMAND_DEADLINE,
    )
    .expect("valid supervisor");
    let key = key("s-3001", "uvwx2345");
    let id = key.service_id();
    // Both supervisors share the namespace and directories, so the fixture's
    // definition names the logs this one expects.
    let definition = fixture.worker(&key, SLEEPING_WORKER, Duration::from_secs(5));
    let domain = format!("gui/{ABSENT_UID}");
    assert!(matches!(
        supervisor.start(&id, &definition).await,
        Err(Error::DomainUnavailable { domain: found }) if found == domain
    ));
    assert!(!fixture.definition_path(&key).exists());
    assert!(matches!(
        supervisor.inspect(&id).await,
        Err(Error::DomainUnavailable { .. })
    ));
    assert!(matches!(
        supervisor.retire(&id).await,
        Err(Error::DomainUnavailable { .. })
    ));
}

#[tokio::test]
async fn exit_timeout_ends_a_term_ignoring_process_group() {
    let fixture = Fixture::new();
    let key = key("s-4001", "yzab2345");
    let id = key.service_id();
    let exit_timeout = Duration::from_secs(2);
    let definition = fixture.worker(&key, STUBBORN_WORKER, exit_timeout);
    fixture
        .supervisor
        .start(&id, &definition)
        .await
        .expect("worker starts");
    let main = wait_for(|| fixture.supervisor.inspect(&id), running)
        .await
        .process
        .expect("running worker has a process");
    let inspector = HostInspector::new();
    let deadline = Instant::now() + SPAWN_WAIT;
    let child = loop {
        let children = inspector.descendants(main.pid).expect("descendants");
        if let Some(child) = children.first() {
            assert_eq!(
                child.pgid, main.pid,
                "the child shares the job's process group"
            );
            break child.identity();
        }
        assert!(Instant::now() < deadline, "worker never spawned its child");
        tokio::time::sleep(POLL).await;
    };

    let started = Instant::now();
    fixture
        .supervisor
        .retire(&id)
        .await
        .expect("worker retires");
    let elapsed = started.elapsed();
    wait_gone(&[main, child], started + exit_timeout + ABSENCE_MARGIN).await;
    // `SIGTERM` is ignored, so only the `ExitTimeOut` `SIGKILL` ends the main
    // process; launchd only sends `SIGTERM` to the rest of the group, so the
    // child ends only because retirement kills the captured group.
    assert!(
        elapsed >= exit_timeout.mul_f32(0.75),
        "retirement took {elapsed:?}, shorter than ExitTimeOut"
    );
}

#[tokio::test]
async fn daemon_agent_installs_replaces_and_uninstalls() {
    let fixture = Fixture::new();
    let agents = fixture.root.join("LaunchAgents");
    let daemon = LaunchdDaemon::new(&fixture.namespace, uid(), agents.clone(), COMMAND_DEADLINE)
        .expect("valid daemon manager");
    let definition = |seconds: &str| {
        JobDefinition::new(JobSpec {
            executable: PathBuf::from("/bin/sleep"),
            arguments: vec![seconds.to_owned()],
            environment: BTreeMap::new(),
            working_directory: fixture.root.clone(),
            logs: Some(pohunek_platform::supervisor::JobLogs {
                stdout: fixture.root.join("daemon.out.log"),
                stderr: fixture.root.join("daemon.err.log"),
            }),
            start_timeout: Duration::from_secs(10),
            exit_timeout: Duration::from_secs(5),
            restart: RestartPolicy::OnFailure {
                throttle: Duration::from_secs(1),
            },
            open_files: 1_024,
        })
        .expect("valid daemon definition")
    };
    let label = fixture.namespace.daemon_label();
    let _cleanup = Bootout(label.clone());
    let path = agents.join(format!("{label}.plist"));

    let first = definition("600");
    daemon.install(&first).await.expect("daemon installs");
    let observation = wait_for(|| daemon.inspect(), running).await;
    assert_eq!(observation.definition, Some(first.facts()));
    let first_process = observation.process.expect("running daemon");
    assert!(matches!(
        daemon.install(&first).await,
        Err(Error::AlreadyRegistered(_))
    ));

    let second = definition("700");
    daemon.replace(&second).await.expect("daemon replaced");
    let observation = wait_for(|| daemon.inspect(), running).await;
    assert_eq!(observation.definition, Some(second.facts()));
    let second_process = observation.process.expect("running daemon");
    assert_ne!(second_process, first_process);
    wait_gone(&[first_process], Instant::now() + ABSENCE_MARGIN).await;

    daemon.uninstall().await.expect("daemon uninstalls");
    assert!(matches!(daemon.inspect().await, Err(Error::NotFound(_))));
    assert!(!path.exists(), "agent definition removed");
    wait_gone(&[second_process], Instant::now() + ABSENCE_MARGIN).await;
    daemon.uninstall().await.expect("absent daemon uninstalls");
}

/// Boots one label out when dropped.
struct Bootout(String);

impl Drop for Bootout {
    fn drop(&mut self) {
        // Teardown only: an already absent label is the desired state.
        let _status = launchctl(&["bootout", &format!("gui/{}/{}", uid(), self.0)]);
    }
}
