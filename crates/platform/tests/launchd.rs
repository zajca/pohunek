//! Exercises the launchd backends against the real `gui/<uid>` domain.
//!
//! Every test derives its own namespace from a fresh temporary directory, so
//! labels never collide with a real installation or a parallel test, and a
//! drop guard boots out every label the test created. The daemon agent tests
//! use a temporary `LaunchAgents` directory; production passes
//! `~/Library/LaunchAgents`, which these tests never touch.
//!
//! The workflow plugin unit tests write into a temporary `LaunchAgents`
//! directory as well and boot out every label they install. Their main
//! process is this test executable, entered through the ignored
//! `fixture_entry` test, which appends `start`/`end` lines to a marker file so
//! the tests observe every run launchd starts.
//!
//! The tests are not ignored: the macOS CI job runs them, and a missing GUI
//! domain fails them instead of skipping.
#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-10-13

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pohunek_platform::process::{HostInspector, ProcessIdentity, ProcessInspector as _};
use pohunek_platform::supervisor::launchd::{LaunchdDaemon, LaunchdPluginUnits, LaunchdSupervisor};
use pohunek_platform::supervisor::{
    Activation, DaemonSupervisor as _, Error, JobDefinition, JobLogs, JobSpec, Namespace,
    PluginUnitDefinition, PluginUnitKey, PluginUnitKind, PluginUnits as _, RestartPolicy,
    ServiceId, ServiceObservation, ServiceState, Supervisor as _, WorkerKey,
};
use pohunek_test_support::wait;

mod support;

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

/// Worker whose process exits on its own shortly after it starts.
///
/// Three seconds leave the 100 ms observation poll ample time to see it run.
/// The background job keeps `bash` itself as the main process; a lone simple
/// command would be `exec`ed and no longer match the definition's executable.
const EXITING_WORKER: &str = "/bin/sleep 3 & wait";

/// Worker that leaves an orphaned copy of itself for launchd to adopt.
///
/// The inner subshell is a fork of `bash` with the worker's argv and never
/// `exec`s (the trailing `true` prevents it); its parent subshell exits, so
/// launchd (PID 1) adopts it and two processes match one job.
const DUPLICATED_WORKER: &str = "( ( /bin/sleep 600; true ) & ); /bin/sleep 600 & wait";

/// Worker that ignores `SIGTERM` and keeps a child in its process group.
const STUBBORN_WORKER: &str = "trap '' TERM; /bin/sleep 600 & wait";

fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn launchctl(arguments: &[&str]) -> std::process::ExitStatus {
    // Empty environment and `/` as the working directory, like the backend's
    // own launchctl runner: the tool addresses launchd through the mach bootstrap
    // port, not through any variable.
    Command::new("/bin/launchctl")
        .env_clear()
        .current_dir("/")
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
        let temporary = pohunek_test_support::tempdir().expect("temporary directory");
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
        // The supervisor creates its private definitions directory only when
        // it first registers a job, so a plant before that creates it `0700`.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.definitions)
            .expect("definitions directory created");
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
async fn an_exited_worker_stays_loaded_without_a_process_and_is_never_proven_ended() {
    let fixture = Fixture::new();
    let key = key("s-1002", "abcd2346");
    let id = key.service_id();
    let definition = fixture.worker(&key, EXITING_WORKER, Duration::from_secs(5));

    fixture
        .supervisor
        .start(&id, &definition)
        .await
        .expect("worker starts");
    wait_for(|| fixture.supervisor.inspect(&id), running).await;
    let observation = wait_for(
        || fixture.supervisor.inspect(&id),
        |observation| observation.process.is_none(),
    )
    .await;

    // launchd keeps the label loaded after the process exits and does not
    // say so; a loaded label without a process is never `Stopped`.
    assert_eq!(observation.state, ServiceState::Unknown);
    assert_eq!(observation.definition, Some(definition.facts()));
    fixture
        .supervisor
        .retire(&id)
        .await
        .expect("worker retires");
    assert!(matches!(
        fixture.supervisor.inspect(&id).await,
        Err(Error::NotFound(missing)) if missing == id
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
    let captured = support::LogCapture::default();
    let observations = {
        let _capture = captured.install();
        fixture.supervisor.discover().await.expect("discovery")
    };
    assert_eq!(observations.len(), 1);
    let mut warned = captured.rejected_entries();
    warned.sort();
    assert_eq!(
        warned, expected,
        "the trait discovery warns about every rejected definition"
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
async fn strict_discovery_refuses_only_rejected_definitions_of_its_namespace() {
    let mut fixture = Fixture::new();
    let other_namespace = Namespace::derive(uid(), &fixture.root.join("other"), &fixture.root);
    let other = other_namespace.worker_label(&key("s-2101", "abcd2345"));
    fixture.plant(&format!("{other}.plist"), b"not a plist", None);
    fixture.plant("com.example.unrelated.plist", b"not a plist", None);
    let malformed = fixture.namespace.worker_label(&key("s-2102", "efgh2345"));
    fixture.plant(&format!("{malformed}.plist"), b"not a plist", None);
    let mismatched = fixture.namespace.worker_label(&key("s-2103", "ijkl2345"));
    fixture.plant(
        &format!("{mismatched}.plist"),
        &plist_document(&other, &["/bin/sleep", "600"]),
        None,
    );
    let unsafe_mode = fixture.namespace.worker_label(&key("s-2104", "mnop2345"));
    fixture.plant(
        &format!("{unsafe_mode}.plist"),
        &plist_document(&unsafe_mode, &["/bin/sleep", "600"]),
        None,
    );
    std::fs::set_permissions(
        fixture.definitions.join(format!("{unsafe_mode}.plist")),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("unsafe definition mode set");

    // A rejected definition of this namespace may belong to a loaded job whose
    // process is never matched, so the destructive discovery refuses it.
    for label in [&malformed, &mismatched, &unsafe_mode] {
        assert!(
            matches!(
                fixture.supervisor.discover_strict().await,
                Err(Error::InvalidData {
                    operation: "discover",
                    ..
                })
            ),
            "{label} must refuse the strict discovery"
        );
        assert!(fixture
            .supervisor
            .discover()
            .await
            .expect("the lenient discovery only warns")
            .is_empty());
        std::fs::remove_file(fixture.definitions.join(format!("{label}.plist")))
            .expect("rejected definition removed");
    }
    // Foreign and other-namespace definitions never refuse it.
    assert!(fixture
        .supervisor
        .discover_strict()
        .await
        .expect("foreign definitions are ignored")
        .is_empty());
}

#[tokio::test]
async fn strict_discovery_holds_a_loaded_worker_whose_definition_is_set_aside() {
    let mut fixture = Fixture::new();
    // No worker was ever registered: the missing directory is an empty registry.
    assert!(!fixture.definitions.exists());
    assert!(fixture
        .supervisor
        .discover_strict()
        .await
        .expect("a missing definitions directory is empty")
        .is_empty());

    // An absent label's set-aside definition runs nothing and stays in place.
    let idle = fixture.namespace.worker_label(&key("s-2201", "abcd2345"));
    let idle_parked = format!(".{idle}.plist.replaced");
    fixture.plant(
        &idle_parked,
        &plist_document(&idle, &["/bin/sleep", "600"]),
        None,
    );
    assert!(fixture
        .supervisor
        .discover_strict()
        .await
        .expect("an absent set-aside worker is not a loaded job")
        .is_empty());
    assert!(fixture.definitions.join(&idle_parked).exists());

    // A crash between setting a loaded job's definition aside and publishing
    // its successor leaves the job loaded without `<label>.plist`.
    let loaded = key("s-2202", "efgh2345");
    let id = loaded.service_id();
    let label = fixture.namespace.worker_label(&loaded);
    fixture.extra_labels.push(label.clone());
    let definition = fixture.worker(&loaded, SLEEPING_WORKER, Duration::from_secs(5));
    fixture
        .supervisor
        .start(&id, &definition)
        .await
        .expect("worker starts");
    wait_for(|| fixture.supervisor.inspect(&id), running).await;
    let path = fixture.definition_path(&loaded);
    let parked = fixture.definitions.join(format!(".{label}.plist.replaced"));
    std::fs::rename(&path, &parked).expect("definition set aside");

    assert!(
        matches!(
            fixture.supervisor.discover_strict().await,
            Err(Error::Race {
                operation: "discover"
            })
        ),
        "a loaded set-aside worker must hold the strict discovery"
    );
    assert!(fixture
        .supervisor
        .discover()
        .await
        .expect("the lenient discovery omits it")
        .is_empty());
    assert!(
        parked.exists(),
        "discovery never moves a set-aside definition"
    );

    std::fs::rename(&parked, &path).expect("definition put back");
    let observations = fixture
        .supervisor
        .discover_strict()
        .await
        .expect("the settled worker is discovered");
    assert_eq!(
        observations
            .iter()
            .map(|observation| &observation.id)
            .collect::<Vec<_>>(),
        [&id]
    );
    fixture
        .supervisor
        .retire(&id)
        .await
        .expect("worker retires");
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
    // The daemon is matched by launchd parentage, executable, and exact argv.
    // A bare `/bin/sleep 600` would also match the foreign jobs a concurrent
    // discovery test loads, so the argv carries this fixture's namespace.
    let definition = |script: &str| {
        JobDefinition::new(JobSpec {
            executable: PathBuf::from("/bin/bash"),
            arguments: vec![
                "-c".to_owned(),
                script.to_owned(),
                "pohunek-test-daemon".to_owned(),
                fixture.namespace.as_str().to_owned(),
            ],
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

    let first = definition(SLEEPING_WORKER);
    daemon.install(&first).await.expect("daemon installs");
    let observation = wait_for(|| daemon.inspect(), running).await;
    assert_eq!(observation.definition, Some(first.facts()));
    let first_process = observation.process.expect("running daemon");
    assert!(matches!(
        daemon.install(&first).await,
        Err(Error::AlreadyRegistered(_))
    ));

    let second = definition("/bin/sleep 700 & wait");
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

#[tokio::test]
async fn daemon_replace_without_an_agent_definition_is_not_found() {
    let fixture = Fixture::new();
    let agents = fixture.root.join("LaunchAgents");
    let daemon = LaunchdDaemon::new(&fixture.namespace, uid(), agents.clone(), COMMAND_DEADLINE)
        .expect("valid daemon manager");
    let definition = JobDefinition::new(JobSpec {
        executable: PathBuf::from("/bin/bash"),
        arguments: vec![
            "-c".to_owned(),
            SLEEPING_WORKER.to_owned(),
            "pohunek-test-daemon".to_owned(),
            fixture.namespace.as_str().to_owned(),
        ],
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
    .expect("valid daemon definition");
    let label = fixture.namespace.daemon_label();
    let _cleanup = Bootout(label.clone());

    let replaced = daemon.replace(&definition).await;
    assert!(matches!(replaced, Err(Error::NotFound(_))), "{replaced:?}");
    assert!(
        !agents.exists(),
        "replace never creates the agents directory"
    );

    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&agents)
        .expect("agents directory");
    let replaced = daemon.replace(&definition).await;
    assert!(matches!(replaced, Err(Error::NotFound(_))), "{replaced:?}");
    assert_eq!(
        std::fs::read_dir(&agents).expect("agents listing").count(),
        0,
        "replace never writes an agent definition"
    );
    assert!(matches!(daemon.inspect().await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn daemon_replace_resumes_after_a_crash_left_the_agent_set_aside() {
    let fixture = Fixture::new();
    let agents = fixture.root.join("LaunchAgents");
    let daemon = LaunchdDaemon::new(&fixture.namespace, uid(), agents.clone(), COMMAND_DEADLINE)
        .expect("valid daemon manager");
    let definition = |script: &str| {
        JobDefinition::new(JobSpec {
            executable: PathBuf::from("/bin/bash"),
            arguments: vec![
                "-c".to_owned(),
                script.to_owned(),
                "pohunek-test-daemon".to_owned(),
                fixture.namespace.as_str().to_owned(),
            ],
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
    // The fixed name `register` sets a replaced definition aside under.
    let parked = agents.join(format!(".{label}.plist.replaced"));
    let listing = || {
        let mut names: Vec<_> = std::fs::read_dir(&agents)
            .expect("agents listing")
            .map(|entry| entry.expect("agent entry").file_name())
            .collect();
        names.sort();
        names
    };

    daemon
        .install(&definition(SLEEPING_WORKER))
        .await
        .expect("daemon installs");
    wait_for(|| daemon.inspect(), running).await;

    // A replacement that crashed after `bootout` and the set-aside leaves the
    // agent stopped with no `<label>.plist`.
    assert!(launchctl(&["bootout", &format!("gui/{}/{label}", uid())]).success());
    std::fs::rename(&path, &parked).expect("agent set aside");
    let second = definition("/bin/sleep 700 & wait");
    daemon.replace(&second).await.expect("replacement resumes");
    let observation = wait_for(|| daemon.inspect(), running).await;
    assert_eq!(observation.definition, Some(second.facts()));
    assert_eq!(
        listing(),
        vec![std::ffi::OsString::from(format!("{label}.plist"))]
    );

    // A crash after publishing leaves both files; the set-aside one is stale.
    std::fs::copy(&path, &parked).expect("stale set-aside copy");
    std::fs::set_permissions(&parked, std::fs::Permissions::from_mode(0o600))
        .expect("private copy");
    let third = definition("/bin/sleep 800 & wait");
    daemon.replace(&third).await.expect("daemon replaced");
    let observation = wait_for(|| daemon.inspect(), running).await;
    assert_eq!(observation.definition, Some(third.facts()));
    assert_eq!(
        listing(),
        vec![std::ffi::OsString::from(format!("{label}.plist"))]
    );

    // Uninstall removes a set-aside definition too, so none survives.
    std::fs::copy(&path, &parked).expect("stale set-aside copy");
    daemon.uninstall().await.expect("daemon uninstalls");
    assert!(listing().is_empty(), "{:?}", listing());
}

/// Boots one label out when dropped.
struct Bootout(String);

impl Drop for Bootout {
    fn drop(&mut self) {
        // Teardown only: an already absent label is the desired state.
        let _status = launchctl(&["bootout", &format!("gui/{}/{}", uid(), self.0)]);
    }
}

#[tokio::test]
async fn retire_refuses_a_job_with_several_matching_processes() {
    let fixture = Fixture::new();
    let key = key("s-5001", "cdef2345");
    let id = key.service_id();
    let definition = fixture.worker(&key, DUPLICATED_WORKER, Duration::from_secs(5));

    fixture
        .supervisor
        .start(&id, &definition)
        .await
        .expect("worker starts");
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        match fixture.supervisor.inspect(&id).await {
            Err(Error::InvalidData { .. }) => break,
            Ok(_) | Err(Error::Race { .. }) => {}
            Err(error) => panic!("inspection failed: {error}"),
        }
        assert!(Instant::now() < deadline, "the orphan was never adopted");
        tokio::time::sleep(POLL).await;
    }

    assert!(matches!(
        fixture.supervisor.retire(&id).await,
        Err(Error::InvalidData {
            operation: "retire",
            ..
        })
    ));
    assert!(
        fixture.definition_path(&key).exists(),
        "an ambiguous job keeps its definition"
    );
    // The drop guard boots the label out; launchd then signals the job's
    // process group, which the orphan still belongs to.
}

#[tokio::test]
async fn start_replaces_a_leftover_definition_of_an_absent_label() {
    let mut fixture = Fixture::new();
    let key = key("s-5002", "ghij2345");
    let id = key.service_id();
    let label = fixture.namespace.worker_label(&key);
    pohunek_platform::filesystem::TrustedDir::open_or_create_absolute(&fixture.definitions, 0o700)
        .expect("private definitions directory");
    fixture.plant(
        &format!("{label}.plist"),
        &plist_document(&label, &["/bin/sleep", "600"]),
        None,
    );
    let definition = fixture.worker(&key, SLEEPING_WORKER, Duration::from_secs(5));

    fixture
        .supervisor
        .start(&id, &definition)
        .await
        .expect("worker starts over a leftover definition");
    let observation = wait_for(|| fixture.supervisor.inspect(&id), running).await;
    assert_eq!(observation.definition, Some(definition.facts()));
    let entries: Vec<_> = std::fs::read_dir(&fixture.definitions)
        .expect("definitions listing")
        .map(|entry| entry.expect("definition entry").file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from(format!("{label}.plist"))]
    );

    fixture
        .supervisor
        .retire(&id)
        .await
        .expect("worker retires");
}

/// Command-line prefix selecting the plugin unit fixture mode.
const FIXTURE_MODE_ARGUMENT: &str = "pohunek-launchd-fixture=";
/// Command-line prefix naming the marker file a plugin unit fixture appends to.
const MARKER_ARGUMENT: &str = "pohunek-launchd-marker=";
/// Command-line prefix naming how long a `job` fixture run sleeps, in ms.
const SLEEP_ARGUMENT: &str = "pohunek-launchd-sleep-ms=";
/// How long a `serve` fixture runs before it exits on its own.
///
/// Far beyond every scenario; each scenario ends the service itself.
const FIXTURE_LIFETIME: Duration = Duration::from_secs(300);
/// Interval of the periodic plugin job fixtures; the shortest the contract
/// accepts.
const JOB_INTERVAL: Duration = Duration::from_secs(1);
/// Timer accuracy of the periodic plugin job fixtures; launchd has no key for
/// it.
const JOB_ACCURACY: Duration = Duration::from_secs(1);
/// Restart delay of the plugin service fixtures; the shortest the contract
/// accepts.
const RESTART_DELAY: Duration = Duration::from_secs(1);
/// Runs observed before a schedule or restart loop counts as repeating.
///
/// launchd has no start limit that parks a job, so a few runs past the first
/// prove the cadence keeps going.
const REPEATED_RUNS: usize = 4;
/// Longest plugin unit name a key accepts.
const LONGEST_NAME: usize = 64;
/// Longest plugin unit label: the reverse-DNS prefix, namespace, `.wf.`, the
/// 12-character plugin hash, the kind, and the longest name.
const LONGEST_LABEL: usize = 119;
/// Longest service ID, which also bounds the plugin unit definition file name.
const LONGEST_FILE_NAME: usize = 128;

/// Child the `stubborn` fixture keeps in its process group; it ignores
/// `SIGTERM`, which `exec` preserves.
const STUBBORN_CHILD: &str = "trap '' TERM; exec /bin/sleep 600";

/// Entry point of the plugin unit fixtures, run by launchd as a job's main
/// process.
///
/// Every mode records a `start` line in the marker file, then: `job` records
/// its full argv next to the marker, sleeps the given time, records `end`, and
/// exits successfully; `crash` exits with status 3; `serve` sleeps;
/// `stubborn` ignores `SIGTERM`, starts a child that ignores it too, and
/// waits for it. The fixture is this test binary rather than a shell because macOS
/// 27 withholds the environment of restricted platform binaries, and plugin
/// unit processes are matched by their environment.
#[test]
#[ignore = "fixture entry point; does work only inside a job started by these tests"]
fn fixture_entry() {
    let Some(mode) = std::env::args().find_map(|argument| {
        argument
            .strip_prefix(FIXTURE_MODE_ARGUMENT)
            .map(str::to_owned)
    }) else {
        return;
    };
    match mode.as_str() {
        "job" => {
            let marker = record_run("start");
            let mut argv = Vec::new();
            for argument in std::env::args_os() {
                argv.extend_from_slice(argument.as_encoded_bytes());
                argv.push(0);
            }
            std::fs::write(argv_record(&marker), argv).expect("record argv");
            let sleep = argument(SLEEP_ARGUMENT)
                .parse()
                .expect("numeric sleep milliseconds");
            // timing-allowed: #326 the run's own length, which lets launchd's interval elapse during a run; no scenario waits on this sleep
            std::thread::sleep(Duration::from_millis(sleep));
            append_line(&marker, "end");
        }
        "crash" => {
            record_run("start");
            std::process::exit(3);
        }
        "serve" => {
            record_run("start");
            // timing-allowed: #326 the fixture's own lifetime; no scenario waits on this sleep, it ends when the scenario stops the job
            std::thread::sleep(FIXTURE_LIFETIME);
        }
        "stubborn" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("signal runtime");
            // A registered handler replaces the default action, so `SIGTERM`
            // no longer ends the process; only `SIGKILL` does.
            let _terminate = runtime
                .block_on(async {
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                })
                .expect("SIGTERM handler");
            let mut child = Command::new("/bin/sh")
                .args(["-c", STUBBORN_CHILD])
                .spawn()
                .expect("stubborn child");
            record_run("start");
            // The run lasts as long as its child; the stop kills both.
            let _status = child.wait();
        }
        other => panic!("unknown fixture mode {other}"),
    }
}

/// Returns the launchd log file of the fixture writing `marker`.
fn log_path(marker: &Path, stream: &str) -> PathBuf {
    let mut path = marker.as_os_str().to_owned();
    path.push(format!(".{stream}.log"));
    PathBuf::from(path)
}

/// Returns the file a `job` fixture records its NUL-terminated argv in.
fn argv_record(marker: &Path) -> PathBuf {
    let mut record = marker.as_os_str().to_owned();
    record.push(".argv");
    PathBuf::from(record)
}

fn argument(prefix: &str) -> String {
    std::env::args()
        .find_map(|argument| argument.strip_prefix(prefix).map(str::to_owned))
        .unwrap_or_else(|| panic!("fixture argument {prefix} is missing"))
}

/// Appends one `<kind> <pid>` line to the marker file and returns its path.
fn record_run(kind: &str) -> PathBuf {
    let marker = PathBuf::from(argument(MARKER_ARGUMENT));
    append_line(&marker, kind);
    marker
}

fn append_line(marker: &Path, kind: &str) {
    use std::io::Write as _;

    // One `write` of a short line to an `O_APPEND` file is atomic, so lines of
    // concurrent runs could interleave only as whole lines, and the file
    // order is the order of the writes.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
        .expect("open marker file")
        .write_all(format!("{kind} {}\n", std::process::id()).as_bytes())
        .expect("append marker line");
}

/// One line of a plugin unit fixture's marker file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    start: bool,
    pid: u32,
}

fn read_marks(marker: &Path) -> Vec<Mark> {
    match std::fs::read_to_string(marker) {
        Ok(text) => text
            .lines()
            .map(|line| {
                let mut fields = line.split(' ');
                let (Some(kind), Some(pid), None) = (fields.next(), fields.next(), fields.next())
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
                }
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read marker file: {error}"),
    }
}

fn count(marks: &[Mark], start: bool) -> usize {
    marks.iter().filter(|mark| mark.start == start).count()
}

/// Returns `launchctl print` output for diagnostics only; it is never matched.
fn print_diagnostics(label: &str) -> String {
    match Command::new("/bin/launchctl")
        .env_clear()
        .current_dir("/")
        .args(["print", &format!("gui/{}/{label}", uid())])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => format!(
            "launchctl print exited with {}:\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => format!("launchctl print could not run: {error}"),
    }
}

/// Returns whether `label` is loaded in `gui/<uid>`.
fn is_loaded(label: &str) -> bool {
    match launchctl(&["print", &format!("gui/{}/{label}", uid())]).code() {
        Some(0) => true,
        Some(113) => false,
        other => panic!(
            "launchctl print of {label} exited with {other:?}; {}",
            print_diagnostics(label)
        ),
    }
}

/// Requires worker discovery to neither adopt, reject, nor warn about the
/// plugin units in its directory.
async fn assert_workers_never_see_plugins(supervisor: &LaunchdSupervisor) {
    let discovery = supervisor
        .discover_definitions()
        .await
        .expect("discover workers");
    assert!(discovery.observations.is_empty(), "{discovery:?}");
    assert!(discovery.rejected.is_empty(), "{discovery:?}");
    assert!(!discovery.incomplete, "{discovery:?}");
    let captured = support::LogCapture::default();
    let (lenient, strict) = {
        let _capture = captured.install();
        (
            supervisor.discover().await.expect("discovery"),
            supervisor
                .discover_strict()
                .await
                .expect("strict discovery"),
        )
    };
    assert!(lenient.is_empty(), "{lenient:?}");
    assert!(strict.is_empty(), "{strict:?}");
    assert!(captured.rejected_entries().is_empty());
}

/// A namespace plus the plugin unit backend writing into a temporary
/// `LaunchAgents` directory.
struct PluginFixture {
    fixture: Fixture,
    agents: PathBuf,
    units: LaunchdPluginUnits,
    /// Labels booted out on drop, whatever state the test left them in.
    labels: Vec<String>,
}

impl PluginFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        let agents = fixture.root.join("LaunchAgents");
        Self::with_agents(fixture, agents)
    }

    fn with_agents(fixture: Fixture, agents: PathBuf) -> Self {
        let units = LaunchdPluginUnits::new(
            fixture.namespace.clone(),
            uid(),
            agents.clone(),
            COMMAND_DEADLINE,
        )
        .expect("valid plugin unit installer");
        Self {
            fixture,
            agents,
            units,
            labels: Vec::new(),
        }
    }

    fn root(&self) -> &Path {
        &self.fixture.root
    }

    /// Registers `key` with the cleanup guard and returns its label.
    fn track(&mut self, key: &PluginUnitKey) -> String {
        let label = self.fixture.namespace.plugin_label(key);
        self.labels.push(label.clone());
        label
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.root().join(format!("{name}.marks"))
    }

    fn definition_path(&self, label: &str) -> PathBuf {
        self.agents.join(format!("{label}.plist"))
    }

    /// Returns the plugin unit entries of this namespace in the agents
    /// directory, including set-aside and temporary names.
    fn plugin_files(&self) -> Vec<String> {
        let segment = format!("{}.wf.", self.fixture.namespace);
        let mut files: Vec<String> = match std::fs::read_dir(&self.agents) {
            Ok(entries) => entries
                .map(|entry| {
                    entry
                        .expect("agents directory entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|name| name.contains(&segment) || name.starts_with(".pohunek-launchd-"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read agents directory: {error}"),
        };
        files.sort();
        files
    }

    /// Reads the written definition of `label` back as a dictionary.
    fn written(&self, label: &str) -> plist::Dictionary {
        let path = self.definition_path(label);
        let mode = std::fs::metadata(&path)
            .unwrap_or_else(|error| panic!("definition of {label}: {error}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "definition mode of {label}");
        plist::Value::from_file(&path)
            .unwrap_or_else(|error| panic!("definition of {label} parses: {error}"))
            .into_dictionary()
            .unwrap_or_else(|| panic!("definition of {label} is a dictionary"))
    }

    async fn wait_for_marks(
        &self,
        label: &str,
        marker: &Path,
        accept: impl Fn(&[Mark]) -> bool,
    ) -> Vec<Mark> {
        let what = format!("the expected runs of {label} in {}", marker.display());
        wait::wait_until(&what, || {
            let marks = read_marks(marker);
            let accepted = accept(&marks);
            async move { accepted.then_some(marks) }
        })
        .await
    }

    async fn wait_for_state(
        &self,
        key: &PluginUnitKey,
        label: &str,
        accept: impl Fn(ServiceState) -> bool,
    ) -> ServiceState {
        let accept = &accept;
        let units = &self.units;
        let what = format!("the expected state of {label}");
        wait::wait_until(&what, move || async move {
            let state = units
                .state(key)
                .await
                .unwrap_or_else(|error| panic!("state of {label}: {error}"));
            accept(state).then_some(state)
        })
        .await
    }

    /// Stops `key`, then starts it twice and waits for a new `start`
    /// (or, with `start` false, `end`) line in `marker` and a running state.
    async fn stop_and_start(&self, key: &PluginUnitKey, marker: &Path, start: bool) {
        let label = self.fixture.namespace.plugin_label(key);
        self.units.stop(key).await.expect("stop plugin unit");
        assert!(!is_loaded(&label), "{label} is booted out");
        assert_eq!(
            self.units.state(key).await.expect("stopped state"),
            ServiceState::Stopped,
            "{label}"
        );
        let before = count(&read_marks(marker), start);
        self.units.start(key).await.expect("start plugin unit");
        self.units.start(key).await.expect("a started unit starts");
        self.wait_for_marks(&label, marker, |marks| count(marks, start) > before)
            .await;
        self.wait_for_state(key, &label, |state| state == ServiceState::Running)
            .await;
    }

    /// A plugin unit job definition running a fixture mode with
    /// bootstrap-only environment.
    fn spec(&self, mode: &str, marker: &Path, sleep: Duration) -> JobSpec {
        JobSpec {
            executable: std::env::current_exe().expect("test executable"),
            arguments: vec![
                "fixture_entry".to_owned(),
                "--exact".to_owned(),
                "--ignored".to_owned(),
                "--test-threads=1".to_owned(),
                format!("{FIXTURE_MODE_ARGUMENT}{mode}"),
                // The marker path is unique per test, so no other test's job
                // matches this argv.
                format!("{MARKER_ARGUMENT}{}", marker.display()),
                format!("{SLEEP_ARGUMENT}{}", sleep.as_millis()),
            ],
            environment: BTreeMap::from([(
                "XDG_STATE_HOME".to_owned(),
                self.root()
                    .join("state")
                    .to_str()
                    .expect("UTF-8 root")
                    .to_owned(),
            )]),
            working_directory: self.root().to_path_buf(),
            // A fixture that fails before it records a run explains itself
            // here; the failing test prints these files.
            logs: Some(JobLogs {
                stdout: log_path(marker, "out"),
                stderr: log_path(marker, "err"),
            }),
            start_timeout: Duration::from_secs(60),
            exit_timeout: Duration::from_secs(5),
            restart: RestartPolicy::Never,
            open_files: 1_024,
        }
    }
}

impl PluginFixture {
    /// Prints every tracked label's launchd state and definition, and the
    /// fixture marker and log files, to the failing test's output.
    ///
    /// Runs while a panic unwinds, so nothing here may panic.
    fn dump_diagnostics(&self) {
        for label in &self.labels {
            eprintln!("--- {label}\n{}", print_diagnostics(label));
            match std::fs::read_to_string(self.definition_path(label)) {
                Ok(definition) => eprintln!("--- definition of {label}\n{definition}"),
                Err(error) => eprintln!("--- definition of {label}: {error}"),
            }
        }
        let Ok(entries) = std::fs::read_dir(self.root()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let dumped = path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| matches!(extension, "marks" | "log" | "argv"));
            if dumped {
                let contents = std::fs::read_to_string(&path).unwrap_or_default();
                eprintln!("--- {}\n{}", path.display(), contents.escape_debug());
            }
        }
    }
}

impl Drop for PluginFixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.dump_diagnostics();
        }
        for label in &self.labels {
            // Teardown only: an already absent label is the desired state.
            let _status = launchctl(&["bootout", &format!("gui/{}/{label}", uid())]);
        }
    }
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

fn restarting_service(mut spec: JobSpec) -> PluginUnitDefinition {
    spec.restart = RestartPolicy::Always {
        delay: RESTART_DELAY,
    };
    PluginUnitDefinition::new(
        JobDefinition::new(spec).expect("valid service definition"),
        Activation::Service,
    )
    .expect("valid plugin service")
}

fn integer(dictionary: &plist::Dictionary, key: &str) -> Option<u64> {
    dictionary
        .get(key)
        .and_then(plist::Value::as_unsigned_integer)
}

fn boolean(dictionary: &plist::Dictionary, key: &str) -> Option<bool> {
    dictionary.get(key).and_then(plist::Value::as_boolean)
}

#[tokio::test]
async fn a_plugin_job_fires_repeatedly() {
    let mut plugin = PluginFixture::new();
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "poll")
        .expect("valid plugin unit key");
    let label = plugin.track(&key);
    let marker = plugin.marker("poll");
    let definition = interval_job(plugin.spec("job", &marker, Duration::ZERO));

    plugin
        .units
        .install(&key, &definition)
        .await
        .expect("install plugin job");
    let marks = plugin
        .wait_for_marks(&label, &marker, |marks| {
            count(marks, false) >= REPEATED_RUNS
        })
        .await;
    let pids: std::collections::BTreeSet<u32> = marks.iter().map(|mark| mark.pid).collect();
    assert!(
        pids.len() >= REPEATED_RUNS,
        "every run of {label} is its own process: {marks:?}"
    );
    assert_eq!(
        plugin.units.state(&key).await.expect("job state"),
        ServiceState::Running,
        "an armed schedule is running; {}",
        print_diagnostics(&label)
    );

    let written = plugin.written(&label);
    assert_eq!(
        written.get("Label").and_then(plist::Value::as_string),
        Some(label.as_str())
    );
    assert_eq!(integer(&written, "StartInterval"), Some(1), "{written:?}");
    // launchd spawns a job at most once per 10 s unless this key is lower.
    assert_eq!(
        integer(&written, "ThrottleInterval"),
        Some(1),
        "{written:?}"
    );
    assert_eq!(boolean(&written, "RunAtLoad"), Some(false), "{written:?}");
    assert!(!written.contains_key("KeepAlive"), "{written:?}");
    assert_eq!(integer(&written, "ExitTimeOut"), Some(5), "{written:?}");
    assert_eq!(
        boolean(&written, "AbandonProcessGroup"),
        Some(false),
        "{written:?}"
    );
    assert!(!written.contains_key("EnableGlobbing"), "{written:?}");
    let environment: Vec<&String> = written
        .get("EnvironmentVariables")
        .and_then(plist::Value::as_dictionary)
        .expect("environment dictionary")
        .keys()
        .collect();
    assert_eq!(environment, ["XDG_STATE_HOME"], "only the bootstrap roots");

    plugin.units.remove(&key).await.expect("remove plugin job");
}

#[tokio::test]
async fn an_active_plugin_job_is_never_started_twice() {
    /// Each run outlasts several intervals, so the interval fires while it
    /// is still active.
    const RUN_LENGTH: Duration = Duration::from_secs(3);
    /// Completed runs whose order is compared.
    const RUNS: usize = 3;

    let mut plugin = PluginFixture::new();
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "slow")
        .expect("valid plugin unit key");
    let label = plugin.track(&key);
    let marker = plugin.marker("slow");
    let definition = interval_job(plugin.spec("job", &marker, RUN_LENGTH));

    plugin
        .units
        .install(&key, &definition)
        .await
        .expect("install plugin job");
    let marks = plugin
        .wait_for_marks(&label, &marker, |marks| count(marks, false) >= RUNS)
        .await;

    // The marker file is in write order, so non-overlapping runs alternate
    // strictly: every `start` is followed by the `end` of the same process
    // before the next `start`.
    for (index, mark) in marks.iter().enumerate() {
        assert_eq!(
            mark.start,
            index % 2 == 0,
            "runs of {label} overlap: {marks:?}"
        );
        if !mark.start {
            assert_eq!(
                marks[index - 1].pid,
                mark.pid,
                "a run of {label} ended inside another: {marks:?}"
            );
        }
    }

    plugin.units.remove(&key).await.expect("remove plugin job");
}

#[tokio::test]
async fn a_crashing_plugin_service_is_restarted() {
    let mut plugin = PluginFixture::new();
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "notifier")
        .expect("valid plugin unit key");
    let label = plugin.track(&key);
    let marker = plugin.marker("notifier");
    let definition = restarting_service(plugin.spec("crash", &marker, Duration::ZERO));

    plugin
        .units
        .install(&key, &definition)
        .await
        .expect("install plugin service");
    let marks = plugin
        .wait_for_marks(&label, &marker, |marks| count(marks, true) >= REPEATED_RUNS)
        .await;
    let pids: std::collections::BTreeSet<u32> = marks.iter().map(|mark| mark.pid).collect();
    assert!(
        pids.len() >= REPEATED_RUNS,
        "every start of {label} is a new process: {marks:?}"
    );
    let state = plugin.units.state(&key).await.expect("service state");
    assert!(
        matches!(state, ServiceState::Running | ServiceState::Unknown),
        "launchd keeps restarting {label}, observed {state:?}"
    );

    let written = plugin.written(&label);
    assert_eq!(boolean(&written, "KeepAlive"), Some(true), "{written:?}");
    assert_eq!(boolean(&written, "RunAtLoad"), Some(true), "{written:?}");
    assert_eq!(
        integer(&written, "ThrottleInterval"),
        Some(1),
        "{written:?}"
    );
    assert!(!written.contains_key("StartInterval"), "{written:?}");
    assert_eq!(
        boolean(&written, "AbandonProcessGroup"),
        Some(false),
        "{written:?}"
    );

    plugin
        .units
        .remove(&key)
        .await
        .expect("remove plugin service");
}

/// launchd has no key bounding a run (`TimeOut` is unimplemented), so a
/// plugin unit's executable bounds its own run; the native backstop is the
/// `ExitTimeOut` grace of a stop, after which the run and its process group
/// are killed.
#[tokio::test]
async fn a_running_plugin_job_that_ignores_sigterm_is_stopped() {
    let mut plugin = PluginFixture::new();
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "wedged")
        .expect("valid plugin unit key");
    let label = plugin.track(&key);
    let marker = plugin.marker("wedged");
    let mut spec = plugin.spec("stubborn", &marker, Duration::ZERO);
    spec.exit_timeout = Duration::from_secs(2);

    plugin
        .units
        .install(&key, &interval_job(spec))
        .await
        .expect("install plugin job");
    let inspector = HostInspector::new();
    let first = plugin
        .wait_for_marks(&label, &marker, |marks| count(marks, true) >= 1)
        .await[0];
    let main = inspector
        .identity(first.pid)
        .expect("process identity")
        .unwrap_or_else(|| panic!("the run of {label} is alive"));
    let child = wait::wait_until(&format!("the child of {label}'s run"), || {
        let child = inspector
            .descendants(main.pid)
            .expect("descendants")
            .first()
            .map(|child| {
                assert_eq!(
                    child.pgid, main.pid,
                    "the child of {label} shares the run's process group"
                );
                child.identity()
            });
        async move { child }
    })
    .await;

    plugin.units.stop(&key).await.expect("stop plugin job");
    // `stop` returns only once the run and its group are gone: `SIGTERM` is
    // ignored, so `ExitTimeOut` ends the run and the captured group.
    for identity in [main, child] {
        assert!(
            !inspector.is_running(identity).expect("liveness"),
            "process {} of {label} outlived the stop",
            identity.pid
        );
    }
    assert!(!is_loaded(&label), "{label} stays booted out");
    assert_eq!(
        plugin.units.state(&key).await.expect("stopped state"),
        ServiceState::Stopped
    );
    assert!(
        plugin.definition_path(&label).exists(),
        "a stopped unit keeps its definition for the next login"
    );

    plugin.units.remove(&key).await.expect("remove plugin job");
}

#[tokio::test]
async fn removed_plugin_units_leave_nothing_and_workers_never_see_them() {
    let fixture = Fixture::new();
    // The plugin definitions share the worker definitions directory, so
    // worker discovery enumerates them and must still ignore them.
    let agents = fixture.definitions.clone();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&agents)
        .expect("definitions directory created");
    let mut plugin = PluginFixture::with_agents(fixture, agents);
    // A unit named `worker` sits closest to the worker label grammar.
    let job = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "worker")
        .expect("valid plugin unit key");
    let service = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "notifier")
        .expect("valid plugin unit key");
    let job_label = plugin.track(&job);
    let service_label = plugin.track(&service);
    let job_marker = plugin.marker("worker");
    let service_marker = plugin.marker("notifier");
    plugin
        .units
        .install(
            &job,
            &interval_job(plugin.spec("job", &job_marker, Duration::ZERO)),
        )
        .await
        .expect("install plugin job");
    plugin
        .units
        .install(
            &service,
            &restarting_service(plugin.spec("serve", &service_marker, Duration::ZERO)),
        )
        .await
        .expect("install plugin service");
    plugin
        .wait_for_marks(&job_label, &job_marker, |marks| count(marks, false) >= 1)
        .await;
    plugin
        .wait_for_marks(&service_label, &service_marker, |marks| {
            count(marks, true) >= 1
        })
        .await;
    plugin
        .wait_for_state(&service, &service_label, |state| {
            state == ServiceState::Running
        })
        .await;

    // Worker discovery of the same namespace and directory ignores them.
    assert_workers_never_see_plugins(&plugin.fixture.supervisor).await;

    // A job proves it started again by completing a new run, a service by a
    // new start; a loaded job reads as running before any run.
    plugin.stop_and_start(&job, &job_marker, false).await;
    plugin.stop_and_start(&service, &service_marker, true).await;

    for key in [&job, &service] {
        plugin.units.remove(key).await.expect("remove plugin unit");
    }
    assert!(
        plugin.plugin_files().is_empty(),
        "{:?}",
        plugin.plugin_files()
    );
    for (key, label) in [(&job, &job_label), (&service, &service_label)] {
        assert!(
            !is_loaded(label),
            "{label} stays loaded; {}",
            print_diagnostics(label)
        );
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
async fn plugin_unit_durations_and_kinds_are_refused_before_install() {
    let mut plugin = PluginFixture::new();
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "poll").expect("valid key");
    plugin.track(&key);
    let marker = plugin.marker("poll");

    // launchd expresses plugin unit durations only in whole seconds of at
    // least one second.
    let job_spec = || plugin.spec("job", &marker, Duration::ZERO);
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
    let other_kind = restarting_service(job_spec());
    assert!(matches!(
        plugin.units.install(&key, &other_kind).await,
        Err(Error::InvalidDefinition { .. })
    ));
    assert!(
        !plugin.agents.exists(),
        "a refused definition writes nothing, not even the directory"
    );

    // The daemon agent keeps refusing to restart after a clean exit.
    let daemon = LaunchdDaemon::new(
        &plugin.fixture.namespace,
        uid(),
        plugin.agents.clone(),
        COMMAND_DEADLINE,
    )
    .expect("valid daemon manager");
    let mut always = job_spec();
    always.restart = RestartPolicy::Always {
        delay: RESTART_DELAY,
    };
    assert!(matches!(
        daemon
            .install(&JobDefinition::new(always).expect("valid definition"))
            .await,
        Err(Error::InvalidDefinition { .. })
    ));
    assert!(!plugin.agents.exists(), "a refused daemon writes nothing");
}

#[tokio::test]
async fn a_plugin_job_and_service_of_one_name_coexist() {
    let mut plugin = PluginFixture::new();
    let job = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "sync").expect("valid key");
    let service =
        PluginUnitKey::new("acme.work", PluginUnitKind::Service, "sync").expect("valid key");
    let job_label = plugin.track(&job);
    let service_label = plugin.track(&service);
    assert_ne!(job_label, service_label);
    let job_marker = plugin.marker("sync-job");
    let replaced_marker = plugin.marker("sync-job-replaced");
    let service_marker = plugin.marker("sync-service");

    plugin
        .units
        .install(
            &job,
            &interval_job(plugin.spec("job", &job_marker, Duration::ZERO)),
        )
        .await
        .expect("install plugin job");
    plugin
        .units
        .install(
            &service,
            &restarting_service(plugin.spec("serve", &service_marker, Duration::ZERO)),
        )
        .await
        .expect("install plugin service of the same name");
    plugin
        .wait_for_marks(&job_label, &job_marker, |marks| count(marks, false) >= 1)
        .await;
    let serving = plugin
        .wait_for_marks(&service_label, &service_marker, |marks| {
            count(marks, true) >= 1
        })
        .await[0];
    let serving = HostInspector::new()
        .identity(serving.pid)
        .expect("process identity")
        .unwrap_or_else(|| panic!("{service_label} runs"));
    let mut expected = vec![
        format!("{job_label}.plist"),
        format!("{service_label}.plist"),
    ];
    expected.sort();
    assert_eq!(plugin.plugin_files(), expected);

    // Replacing the job's definition takes effect and leaves the service
    // process alone.
    plugin
        .units
        .install(
            &job,
            &interval_job(plugin.spec("job", &replaced_marker, Duration::ZERO)),
        )
        .await
        .expect("replace the plugin job");
    plugin
        .wait_for_marks(&job_label, &replaced_marker, |marks| {
            count(marks, false) >= 1
        })
        .await;
    assert!(
        HostInspector::new().is_running(serving).expect("liveness"),
        "replacing {job_label} stopped {service_label}"
    );
    assert_eq!(plugin.plugin_files(), expected, "no set-aside file remains");

    // Removing the service leaves the job's schedule running.
    plugin
        .units
        .remove(&service)
        .await
        .expect("remove plugin service");
    let runs = count(&read_marks(&replaced_marker), false);
    plugin
        .wait_for_marks(&job_label, &replaced_marker, |marks| {
            count(marks, false) > runs
        })
        .await;
    assert_eq!(plugin.plugin_files(), [format!("{job_label}.plist")]);
    assert!(matches!(
        plugin.units.state(&service).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        plugin.units.state(&job).await.expect("job state"),
        ServiceState::Running
    );

    plugin.units.remove(&job).await.expect("remove plugin job");
    assert!(plugin.plugin_files().is_empty());
}

#[tokio::test]
async fn plugin_unit_labels_stay_bounded_and_safe() {
    let mut plugin = PluginFixture::new();
    // The plugin ID carries every character a label must never hold; it
    // reaches launchd only as its hash.
    let hostile = "acme/../$HOME ~%h;\"q\"\\ \u{17e}\n.@";
    let plugin_id = hostile.repeat(4_096 / hostile.len());
    assert!(plugin_id.len() <= 4_096);
    let name: String = std::iter::once('j')
        .chain("_9".chars().cycle().take(LONGEST_NAME - 1))
        .collect();
    assert_eq!(name.len(), LONGEST_NAME);
    let key = PluginUnitKey::new(&plugin_id, PluginUnitKind::Job, name.clone())
        .expect("longest valid key");
    let label = plugin.track(&key);
    let marker = plugin.marker("longest");

    for (plugin_id, name) in [
        ("acme.work", format!("{name}x")),
        ("acme.work", "Upper".to_owned()),
        ("acme.work", "has.dot".to_owned()),
        ("acme.work", "has-dash".to_owned()),
        ("acme.work", "a/b".to_owned()),
        ("acme.work", "9lead".to_owned()),
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

    assert_eq!(label.len(), LONGEST_LABEL, "{label}");
    let file_name = format!("{label}.plist");
    assert!(file_name.len() <= LONGEST_FILE_NAME, "{file_name}");
    assert!(
        label.starts_with(&format!(
            "io.github.zajca.pohunek.{}.wf.",
            plugin.fixture.namespace
        )),
        "{label}"
    );
    assert!(label.ends_with(&format!(".j.{name}")), "{label}");
    assert!(
        label.bytes().all(|byte| byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || b"._".contains(&byte)),
        "{label}"
    );
    ServiceId::parse(label.as_str()).expect("labels are valid service IDs");

    plugin
        .units
        .install(
            &key,
            &interval_job(plugin.spec("job", &marker, Duration::ZERO)),
        )
        .await
        .expect("install the longest plugin unit");
    assert_eq!(plugin.plugin_files(), [file_name]);
    plugin
        .wait_for_marks(&label, &marker, |marks| count(marks, false) >= 1)
        .await;
    assert_eq!(
        plugin.units.state(&key).await.expect("job state"),
        ServiceState::Running
    );

    plugin.units.remove(&key).await.expect("remove plugin job");
    assert!(plugin.plugin_files().is_empty());
}

#[tokio::test]
async fn special_characters_reach_a_plugin_unit_verbatim() {
    let mut plugin = PluginFixture::new();
    let key = PluginUnitKey::new("acme.work", PluginUnitKind::Job, "dollar").expect("valid key");
    let label = plugin.track(&key);
    // launchd passes `ProgramArguments` to `execv` unchanged: no variable,
    // tilde, specifier, or glob expansion.
    let directory = plugin.root().join("bin$HOME${HOME}$$~%h*");
    std::fs::create_dir(&directory).expect("special-character directory");
    let executable = directory.join("fixture");
    std::fs::copy(
        std::env::current_exe().expect("test executable"),
        &executable,
    )
    .expect("copy fixture executable");
    let marker = plugin.root().join("marks-$HOME-${HOME}-$$-~-%h-*.marks");
    // A sibling for the glob argument to match if launchd globbed argv.
    std::fs::write(plugin.root().join("glob-sibling"), b"").expect("glob sibling");
    let mut spec = plugin.spec("job", &marker, Duration::ZERO);
    spec.executable = executable.clone();
    spec.arguments.extend([
        "~/x".to_owned(),
        format!("{}/*", plugin.root().display()),
        "$HOME".to_owned(),
    ]);
    // Only `ProgramArguments` is under test; the logs keep plain names.
    spec.logs = Some(JobLogs {
        stdout: plugin.root().join("dollar.out.log"),
        stderr: plugin.root().join("dollar.err.log"),
    });
    let expected: Vec<String> = std::iter::once(executable.to_str().expect("UTF-8").to_owned())
        .chain(spec.arguments.iter().cloned())
        .collect();

    plugin
        .units
        .install(&key, &interval_job(spec))
        .await
        .expect("install plugin job");
    plugin
        .wait_for_marks(&label, &marker, |marks| count(marks, false) >= 1)
        .await;
    let recorded = std::fs::read_to_string(argv_record(&marker)).expect("recorded argv");
    let argv: Vec<&str> = recorded
        .strip_suffix('\0')
        .expect("NUL-terminated argv")
        .split('\0')
        .collect();
    assert_eq!(
        argv, expected,
        "the argv of {label} is ProgramArguments, unexpanded"
    );

    plugin.units.remove(&key).await.expect("remove plugin job");
}

/// Plugin unit argv need not name the installation, so two units running the
/// same argv are told apart by launchd's label in their environment.
#[tokio::test]
async fn plugin_units_with_identical_argv_are_told_apart_by_label() {
    let mut plugin = PluginFixture::new();
    let first = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "twin_a")
        .expect("valid plugin unit key");
    let second = PluginUnitKey::new("acme.work", PluginUnitKind::Service, "twin_b")
        .expect("valid plugin unit key");
    let first_label = plugin.track(&first);
    let second_label = plugin.track(&second);
    // One marker for both, so the two definitions carry the same argv.
    let marker = plugin.marker("twins");
    let definition = restarting_service(plugin.spec("serve", &marker, Duration::ZERO));
    for key in [&first, &second] {
        plugin
            .units
            .install(key, &definition)
            .await
            .expect("install plugin service");
    }
    let marks = plugin
        .wait_for_marks(&first_label, &marker, |marks| count(marks, true) >= 2)
        .await;
    let inspector = HostInspector::new();
    let processes: Vec<ProcessIdentity> = marks
        .iter()
        .map(|mark| {
            inspector
                .identity(mark.pid)
                .expect("process identity")
                .unwrap_or_else(|| panic!("process {} of the twins is alive", mark.pid))
        })
        .collect();
    assert_eq!(processes.len(), 2, "{marks:?}");
    for (key, label) in [(&first, &first_label), (&second, &second_label)] {
        plugin
            .wait_for_state(key, label, |state| state == ServiceState::Running)
            .await;
    }

    plugin
        .units
        .stop(&first)
        .await
        .expect("stop the first twin");
    let alive: Vec<&ProcessIdentity> = processes
        .iter()
        .filter(|identity| inspector.is_running(**identity).expect("liveness"))
        .collect();
    assert_eq!(
        alive.len(),
        1,
        "stopping {first_label} must end its own process and leave \
         {second_label}'s alone: {processes:?}"
    );
    assert_eq!(
        plugin.units.state(&first).await.expect("first state"),
        ServiceState::Stopped
    );
    assert_eq!(
        plugin.units.state(&second).await.expect("second state"),
        ServiceState::Running,
        "{}",
        print_diagnostics(&second_label)
    );

    for key in [&first, &second] {
        plugin
            .units
            .remove(key)
            .await
            .expect("remove plugin service");
    }
    assert!(
        processes
            .iter()
            .all(|identity| !inspector.is_running(*identity).expect("liveness")),
        "removal ends both twins"
    );
}
