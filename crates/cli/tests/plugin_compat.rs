//! `pohunek plugin compat` and the `runtime_api` / `runtime_features` fields of
//! `host.inspect`, against a real `pohunekd` subprocess.
//!
//! The expected feature set comes from sources outside the daemon code: the
//! committed registry snapshot (`fixtures/features.txt`) and the `requires`
//! lists of the shipped `runtime-packages/*/runtime.toml`. The N-1 daemon (one
//! that omits the fields) is covered in `plugin_cli.rs`.

// Rust guideline compliant 2026-11-21

#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{poll_until, HANG_GUARD};
use pohunek_test_support::workers::WorkerGuard;
use pohunek_test_support::{bin_exe, worker_binary};
use serde_json::Value;

/// Control socket of the daemon below the runtime directory.
const SOCKET: &str = "pohunek/daemon.sock";

/// Shell the daemon hands to terminal sessions; it only has to exist.
const SHELL: &str = "/bin/sh";

/// Name of the daemon binary beside the CLI binary.
const DAEMON_NAME: &str = "pohunekd";

/// The committed snapshot of the feature registry, one `<id> <since>` line per
/// feature. A daemon test pins the registry to it, so it is a source of its own.
const FEATURE_SNAPSHOT: &str = "crates/daemon/src/agent/host/fixtures/features.txt";

/// A reply of a daemon from before the compat report: the keys `host.inspect`
/// answered at release 0.44.0, which has neither `runtime_api` nor
/// `runtime_features`.
const N_MINUS_1_REPLY: &str = include_str!("fixtures/host-inspect-n1.json");

/// A running daemon and the hermetic environment the CLI runs in.
struct Daemon {
    // Declared before `env` so the workers are reaped while the root exists.
    _workers: WorkerGuard,
    env: TestEnv,
    child: Option<Child>,
}

impl Daemon {
    fn start() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let workers = WorkerGuard::watch(env.root().to_path_buf());
        let out = File::create(env.root().join("daemon.out")).expect("create the daemon log");
        let err = out.try_clone().expect("duplicate the daemon log handle");
        let mut command = env.command(daemon_binary());
        command
            .env("SHELL", SHELL)
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        let child = command.spawn().expect("spawn pohunekd");
        let mut daemon = Self {
            _workers: workers,
            env,
            child: Some(child),
        };
        let socket = daemon.env.runtime_dir().join(SOCKET);
        poll_until("the daemon control socket accepts connections", || {
            if let Some(status) = daemon
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
            {
                panic!(
                    "the daemon exited before readiness ({status}):\n{}",
                    daemon.log()
                );
            }
            UnixStream::connect(&socket).ok().map(drop)
        });
        daemon
    }

    fn log(&self) -> String {
        fs::read_to_string(self.env.root().join("daemon.out")).unwrap_or_default()
    }

    /// Runs the CLI and returns its output.
    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = self.env.command(bin_exe("pohunek"));
        command.args(arguments);
        bounded(command, &format!("pohunek {arguments:?}"))
    }

    /// Runs `pohunek <arguments> --json`, which must succeed, and returns its
    /// `ok` payload.
    fn ok(&self, arguments: &[&str]) -> Value {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self.run(&all);
        let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                self.log()
            )
        });
        assert_eq!(output.status.code(), Some(0), "{all:?} failed: {document}");
        document["ok"].clone()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("daemon output before the failure:\n{}", self.log());
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Path of the `pohunekd` binary beside the CLI binary.
fn daemon_binary() -> PathBuf {
    bin_exe("pohunek").with_file_name(DAEMON_NAME)
}

/// Runs `command` to completion, failing with `what` after [`HANG_GUARD`].
fn bounded(mut command: Command, what: &str) -> Output {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    rx.recv_timeout(HANG_GUARD)
        .unwrap_or_else(|_| panic!("{what} did not finish within {HANG_GUARD:?}"))
        .unwrap_or_else(|error| panic!("{what} could not run: {error}"))
}

fn repository_root() -> PathBuf {
    pohunek_test_support::workspace_root()
}

/// The feature ids of the committed registry snapshot.
fn snapshot_features() -> Vec<String> {
    let snapshot = fs::read_to_string(repository_root().join(FEATURE_SNAPSHOT))
        .expect("read the feature snapshot");
    let ids: Vec<String> = snapshot
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    assert!(!ids.is_empty(), "the feature snapshot has rows");
    ids
}

/// Every feature id the shipped runtime packages list in `requires`.
fn required_by_shipped_packages() -> BTreeSet<String> {
    let root = repository_root().join("runtime-packages");
    let mut required = BTreeSet::new();
    for entry in fs::read_dir(&root).expect("read runtime-packages") {
        let descriptor = entry
            .expect("a directory entry")
            .path()
            .join("runtime.toml");
        if !descriptor.is_file() {
            continue;
        }
        let document: toml::Table = fs::read_to_string(&descriptor)
            .expect("read a runtime descriptor")
            .parse()
            .expect("a runtime descriptor is TOML");
        if let Some(requires) = document.get("requires").and_then(toml::Value::as_array) {
            required.extend(
                requires
                    .iter()
                    .map(|id| id.as_str().expect("a feature id is a string").to_owned()),
            );
        }
    }
    required
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("a JSON array, got {value}"))
        .iter()
        .map(|item| item.as_str().expect("a string").to_owned())
        .collect()
}

/// Rows 2, 3 and 6 of #1659 D3: the report of a running daemon names runtime
/// API 1, the core version `pohunekd --version` prints and exactly the
/// snapshot's features, a superset of everything the shipped packages require.
#[test]
fn plugin_compat_reports_the_core_contract() {
    let daemon = Daemon::start();

    let report = daemon.ok(&["plugin", "compat"]);

    assert_eq!(report["runtime_api"], 1);
    let printed = bounded(
        {
            let mut command = Command::new(daemon_binary());
            command.arg("--version");
            command
        },
        "pohunekd --version",
    );
    let printed = String::from_utf8_lossy(&printed.stdout).into_owned();
    let core_version = report["core_version"].as_str().expect("a core version");
    assert!(
        printed.split_whitespace().any(|word| word == core_version),
        "`pohunekd --version` ({printed:?}) names the reported core version {core_version:?}"
    );
    assert!(
        report["protocol_version"].is_number() || report["protocol_version"].is_string(),
        "the report names the protocol version: {report}"
    );

    let reported = strings(&report["features"]);
    assert_eq!(
        reported,
        snapshot_features(),
        "the report lists the snapshot's features in registry order"
    );
    let required = required_by_shipped_packages();
    assert!(
        !required.is_empty(),
        "the shipped packages require features, so the subset check is not vacuous"
    );
    let reported_set: BTreeSet<String> = reported.into_iter().collect();
    let missing: Vec<_> = required.difference(&reported_set).collect();
    assert!(
        missing.is_empty(),
        "features a shipped package requires but the core does not report: {missing:?}"
    );
}

/// Row 1 of #1659 D3: the full `host.inspect` answer carries the same two
/// fields as the compat report.
#[test]
fn host_inspect_carries_the_runtime_report() {
    let daemon = Daemon::start();

    let inspected = daemon.ok(&["host", "inspect", "local"]);
    let compat = daemon.ok(&["plugin", "compat"]);

    assert_eq!(inspected["runtime_api"], 1);
    assert_eq!(
        strings(&inspected["runtime_features"]),
        strings(&compat["features"])
    );
    assert_eq!(inspected["daemon_version"], compat["core_version"]);
}

/// Row 5 of #1659 D3: an old client decodes the new daemon's `host.inspect`
/// unchanged. The answer adds the two optional keys to what an N-1 answer
/// can hold and nothing else, and the old client ignores keys it does not
/// know (`HostCapabilities` has no `deny_unknown_fields`).
#[test]
fn host_inspect_adds_only_the_two_optional_keys() {
    let daemon = Daemon::start();

    let inspected = daemon.ok(&["host", "inspect", "local"]);

    let old: Value = serde_json::from_str(N_MINUS_1_REPLY).expect("the N-1 reply is JSON");
    let old_keys: BTreeSet<&str> = old
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    let new_keys: BTreeSet<&str> = inspected
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    let added: Vec<&str> = new_keys.difference(&old_keys).copied().collect();
    assert_eq!(added, ["runtime_api", "runtime_features"]);

    // The new client decodes the N-1 answer and writes it back unchanged.
    let decoded: protocol::HostCapabilities =
        serde_json::from_value(old.clone()).expect("the N-1 reply decodes");
    assert_eq!(
        serde_json::to_value(&decoded).expect("the decoded reply serializes"),
        old
    );
}
