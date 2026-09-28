//! Real-process proof of the worker child-environment contract.
//!
//! A real `pohunek-sessiond` runs with the variables systemd and launchd give a
//! supervised job plus an unrelated variable, and a real PTY child prints its
//! environment. Protocol version six must hand the child exactly the base
//! environment, the worker's `TERM`, the profile, and the worker identity. A
//! version-five session keeps inheriting the worker environment, but without
//! service-manager variables.
//!
//! Build the worker first: `cargo build -p pohunek-session-worker --bin
//! pohunek-sessiond`.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_daemon::runtime::Worker;
use pohunek_worker_protocol::{
    is_denylisted, BaseEnv, Dimensions, Initialize, InitializeLimits, LaunchIdentity, RuntimePhase,
    SecretEnv, SessionId, StopPolicy, TransactionId, Version, BASE_ENVIRONMENT_VERSION,
    PREVIOUS_VERSION,
};

/// Bounds waiting for the worker socket and the child's exit.
const DEADLINE: Duration = Duration::from_secs(15);
/// Pause between readiness and exit polls.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Output page size; an environment listing fits in a few pages.
const PAGE_BYTES: u32 = 16 * 1024;
/// Short observation wait, because the child has already exited.
const PAGE_WAIT: Duration = Duration::from_millis(100);
/// Daemon identity presented to the worker.
const DAEMON_ID: &str = "daemon-environment-test";
/// Generation passed on the worker command line.
const GENERATION: &str = "abcd2345";
/// Value of the variable no allowlist names; it must never reach a v6 child.
const UNRELATED_VALUE: &str = "worker-only-value";

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Variables a service manager sets on the worker it supervises.
const SERVICE_MANAGER_VARIABLES: &[(&str, &str)] = &[
    ("WATCHDOG_USEC", "30000000"),
    ("WATCHDOG_PID", "1"),
    ("INVOCATION_ID", "0123456789abcdef0123456789abcdef"),
    ("JOURNAL_STREAM", "8:12345"),
    ("MANAGERPID", "1"),
    ("SYSTEMD_EXEC_PID", "1"),
    ("XPC_SERVICE_NAME", "io.github.zajca.pohunek.test.worker"),
    ("XPC_FLAGS", "0x0"),
    ("__CFBundleIdentifier", "io.github.zajca.pohunek"),
    ("LaunchInstanceID", "00000000-0000-0000-0000-000000000000"),
    ("POHUNEK_BOOTSTRAP_TOKEN", "bootstrap-token"),
    ("POHUNEK_CONTROLLER_TOKEN", "controller-token"),
];

struct Fixture {
    root: PathBuf,
    session_id: String,
    notify: UnixDatagram,
    worker: tokio::process::Child,
}

impl Fixture {
    /// Starts a real worker whose own environment is fully controlled.
    fn start() -> Self {
        let binary = worker_binary();
        assert!(
            binary.is_file(),
            "{} is missing; build pohunek-sessiond first",
            binary.display()
        );
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = pohunek_test_support::temp_root()
            .join(format!("pohunek-env-{}-{sequence}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for directory in [
            "runtime/pohunek/workers",
            "state/pohunek/workers",
            "data/pohunek",
            "config/pohunek",
            "cache/pohunek",
            "home",
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(directory))
                .expect("create fixture directory");
        }
        let notify_path = root.join("notify.sock");
        let notify = UnixDatagram::bind(&notify_path).expect("bind notify socket");
        notify
            .set_read_timeout(Some(DEADLINE))
            .expect("notify read timeout");
        let session_id = format!("s-{}{sequence}", std::process::id());

        let mut command = tokio::process::Command::new(binary);
        command
            .args(["--session-id", &session_id])
            .args(["--worker-generation", GENERATION])
            .arg("--daemon-socket-path")
            .arg(root.join("runtime/pohunek/daemon.sock"))
            .env_clear()
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("HOME", root.join("home"))
            .env("NOTIFY_SOCKET", &notify_path)
            .env("UNRELATED_WORKER_VAR", UNRELATED_VALUE)
            .envs(SERVICE_MANAGER_VARIABLES.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let worker = command.spawn().expect("spawn pohunek-sessiond");
        Self {
            root,
            session_id,
            notify,
            worker,
        }
    }

    async fn connect(&mut self, maximum_version: Version) -> Worker {
        // The worker reads NOTIFY_SOCKET from its own environment, proving
        // the service-manager variables really were present in the worker.
        let mut ready = [0_u8; 256];
        let length = self.notify.recv(&mut ready).expect("worker readiness");
        assert!(ready[..length].starts_with(b"READY=1"));

        let socket = self
            .root
            .join("runtime/pohunek/workers")
            .join(&self.session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let started = tokio::time::Instant::now();
        loop {
            match Worker::connect_with_range(
                &socket,
                &self.session_id,
                DAEMON_ID,
                PREVIOUS_VERSION,
                maximum_version,
            )
            .await
            {
                Ok(worker) => return worker,
                Err(error) => {
                    assert!(
                        self.worker.try_wait().expect("inspect worker").is_none(),
                        "worker exited before accepting a controller: {error}"
                    );
                    assert!(
                        started.elapsed() < DEADLINE,
                        "worker never accepted: {error}"
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    fn initialize(&self, worker_id: pohunek_worker_protocol::WorkerId) -> Initialize {
        Initialize {
            session_id: SessionId::new(&self.session_id).expect("session id"),
            transaction_id: TransactionId::new("create-environment").expect("transaction id"),
            expected_worker_id: worker_id,
            launch: LaunchIdentity {
                agent: "codex".to_owned(),
                agent_base: "codex".to_owned(),
                reference_kind: Some("id".to_owned()),
            },
            executable: PathBuf::from("/usr/bin/env"),
            arguments: Vec::new(),
            cwd: self.root.clone(),
            // Wide enough that no environment line wraps in the PTY.
            dimensions: Dimensions::new(1000, 50).expect("dimensions"),
            environment: SecretEnv::new(BTreeMap::from([
                ("PROFILE_TOKEN".to_owned(), "profile-value".to_owned()),
                ("NOTIFY_SOCKET".to_owned(), "/run/profile-notify".to_owned()),
            ]))
            .expect("profile environment"),
            base_environment: Some(
                BaseEnv::new(BTreeMap::from([
                    ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                    ("HOME".to_owned(), "/home/base".to_owned()),
                    ("LANG".to_owned(), "C.UTF-8".to_owned()),
                    ("SHELL".to_owned(), "/bin/sh".to_owned()),
                ]))
                .expect("base environment"),
            ),
            limits: InitializeLimits::new(1_048_576, 1_048_576, 128, 60_000).expect("limits"),
            stop_policy: StopPolicy::new(500).expect("stop policy"),
            hook_protocol_version: Version::new(1).expect("hook version"),
            public_protocol_version: 7,
        }
    }

    fn socket_path(&self) -> String {
        self.root
            .join("runtime/pohunek/workers")
            .join(&self.session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME)
            .to_str()
            .expect("UTF-8 fixture path")
            .to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.worker.start_kill();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Runs `/usr/bin/env` in the worker's PTY and returns the child environment.
async fn child_environment(worker: &Worker, initialize: Initialize) -> BTreeMap<String, String> {
    worker.initialize(initialize).await.expect("initialize");
    let started = tokio::time::Instant::now();
    loop {
        let snapshot = worker.inspect().await.expect("inspect worker");
        if snapshot.phase == RuntimePhase::Exited {
            assert_eq!(snapshot.exit.and_then(|exit| exit.code), Some(0));
            break;
        }
        assert!(started.elapsed() < DEADLINE, "child never exited");
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let mut output = Vec::new();
    let mut after_offset = None;
    loop {
        let page = worker
            .read_output(after_offset, PAGE_BYTES, PAGE_WAIT)
            .await
            .expect("read child output");
        assert!(page.gap.is_none(), "environment listing must be retained");
        output.extend_from_slice(page.data.expose());
        after_offset = Some(page.next_offset);
        if !page.has_more {
            break;
        }
    }
    String::from_utf8(output)
        .expect("environment listing is UTF-8")
        .split("\r\n")
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once('=').expect("NAME=value line");
            (name.to_owned(), value.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn version_six_child_gets_only_base_term_profile_and_identity() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(BASE_ENVIRONMENT_VERSION).await;
    let worker_id = worker.worker_id().await;
    let initialize = fixture.initialize(worker_id.clone());

    let environment = child_environment(&worker, initialize).await;

    let runtime_id = worker.runtime_id().await.expect("runtime id").to_string();
    let socket_path = fixture.socket_path();
    let daemon_socket = fixture.root.join("runtime/pohunek/daemon.sock");
    let expected = BTreeMap::from([
        ("HOME", "/home/base"),
        ("LANG", "C.UTF-8"),
        ("PATH", "/usr/bin:/bin"),
        ("SHELL", "/bin/sh"),
        ("TERM", "xterm-256color"),
        ("PROFILE_TOKEN", "profile-value"),
        ("POHUNEK_ENV", "1"),
        ("POHUNEK_NATIVE_REFERENCE_KIND", "id"),
        ("POHUNEK_PROTOCOL_VERSION", "7"),
        ("POHUNEK_RUNTIME_ID", runtime_id.as_str()),
        ("POHUNEK_SESSION_ID", fixture.session_id.as_str()),
        (
            "POHUNEK_SOCKET_PATH",
            daemon_socket.to_str().expect("UTF-8 fixture path"),
        ),
        ("POHUNEK_WORKER_HOOK_PROTOCOL_VERSION", "1"),
        ("POHUNEK_WORKER_ID", worker_id.as_str()),
        ("POHUNEK_WORKER_SOCKET_PATH", socket_path.as_str()),
    ])
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect::<BTreeMap<_, _>>();
    assert_eq!(environment, expected);
}

#[tokio::test]
async fn version_five_child_inherits_the_worker_without_service_manager_variables() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(PREVIOUS_VERSION).await;
    let initialize = fixture.initialize(worker.worker_id().await);

    let environment = child_environment(&worker, initialize).await;

    assert_eq!(
        environment.get("UNRELATED_WORKER_VAR").map(String::as_str),
        Some(UNRELATED_VALUE),
        "a version-five child keeps inheriting the worker environment"
    );
    assert_eq!(
        environment.get("PROFILE_TOKEN").map(String::as_str),
        Some("profile-value")
    );
    assert_eq!(
        environment.get("POHUNEK_SESSION_ID"),
        Some(&fixture.session_id)
    );
    assert!(
        !environment.contains_key("TERM"),
        "only version six sets TERM"
    );
    assert!(
        environment
            .get("HOME")
            .is_some_and(|home| home != "/home/base"),
        "a version-five worker never receives the base environment"
    );
    for name in environment.keys() {
        assert!(!is_denylisted(name), "{name} leaked to the child");
    }
    assert!(!environment.contains_key("NOTIFY_SOCKET"));
}

fn worker_binary() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .expect("daemon crate is inside workspace")
                .join("target")
        },
        PathBuf::from,
    );
    target.join("debug/pohunek-sessiond")
}
