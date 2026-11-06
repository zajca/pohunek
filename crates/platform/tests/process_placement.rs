//! Exercises the POSIX-session and listening-socket probes of the host process
//! inspector against real processes.
//!
//! The fixture image is this test binary, re-run with `--ignored` and a mode in
//! its environment. A daemon-shaped image leaves the scenario's session with
//! `setsid` and listens on a unix socket; a tree-shaped image stays in the
//! scenario's session and holds no socket. The expected answers come from the
//! shape of each fixture, never from the inspector under test.
#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-11-06

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pohunek_platform::process::{HostInspector, Pid, ProcessInspector as _};
use pohunek_test_support::wait::poll_until;

/// Environment variable naming the scenario directory the image publishes into.
const DIR_ENV: &str = "PLACEMENT_FIXTURE_DIR";

/// Environment variable naming the image's shape.
const MODE_ENV: &str = "PLACEMENT_FIXTURE_MODE";

/// Shape that leaves the session and listens.
const DAEMON_MODE: &str = "daemon";

/// Shape that stays in the session and holds no socket.
const TREE_MODE: &str = "tree";

/// File the image creates once it has taken its shape.
const READY_FILE: &str = "ready";

/// Directory whose removal ends the image.
const BARRIER_DIR: &str = "barrier";

/// Socket file a daemon-shaped image listens on.
const SOCKET_FILE: &str = "s";

/// Upper bound on how long an image lives when its barrier is never removed.
const IMAGE_LIFETIME: Duration = Duration::from_secs(300);

/// Pause between the image's checks of its barrier.
const IMAGE_POLL: Duration = Duration::from_millis(10);

/// Body of the fixture image: takes the shape in [`MODE_ENV`], publishes
/// [`READY_FILE`] and lives while [`BARRIER_DIR`] exists.
///
/// Only the scenarios start it, so a normal run returns at once.
#[test]
#[ignore = "image of the fixture processes spawned by the placement scenarios"]
fn image() {
    let (Some(mode), Some(dir)) = (std::env::var_os(MODE_ENV), std::env::var_os(DIR_ENV)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _listener = (mode == DAEMON_MODE).then(|| {
        rustix::process::setsid().expect("leave the session");
        std::os::unix::net::UnixListener::bind(dir.join(SOCKET_FILE))
            .expect("listen on a unix socket")
    });
    std::fs::write(dir.join(READY_FILE), b"").expect("publish readiness");
    let started = std::time::Instant::now();
    while dir.join(BARRIER_DIR).is_dir() && started.elapsed() < IMAGE_LIFETIME {
        // timing-allowed: #1947 the image's own liveness poll; no scenario waits on this sleep, the barrier ends it
        std::thread::sleep(IMAGE_POLL);
    }
}

/// A running fixture image and the directory it publishes into.
struct Fixture {
    child: Child,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn start(mode: &str) -> Self {
        let dir = pohunek_test_support::tempdir().expect("scenario directory");
        std::fs::create_dir(dir.path().join(BARRIER_DIR)).expect("create the barrier");
        let child = Command::new(std::env::current_exe().expect("this test binary"))
            .args(["image", "--exact", "--ignored", "--nocapture"])
            .env(MODE_ENV, mode)
            .env(DIR_ENV, dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the fixture image");
        let fixture = Self { child, dir };
        poll_until("the image to take its shape", || {
            fixture.dir.path().join(READY_FILE).exists().then_some(())
        });
        fixture
    }

    fn pid(&self) -> Pid {
        self.child.id()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(Path::new(self.dir.path()).join(BARRIER_DIR));
        let _ = self.child.wait();
    }
}

#[test]
fn a_daemon_shaped_process_is_in_another_session_and_listens() {
    let inspector = HostInspector::new();
    let daemon = Fixture::start(DAEMON_MODE);

    let own = inspector
        .process_session(std::process::id())
        .expect("read the scenario's session")
        .expect("the scenario runs");
    let found = inspector
        .process_session(daemon.pid())
        .expect("read the image's session")
        .expect("the image runs");

    assert_ne!(found, own, "setsid left the scenario's session");
    assert_eq!(
        inspector
            .holds_listener(daemon.pid())
            .expect("read the image's sockets"),
        Some(true)
    );
}

#[test]
fn a_tree_shaped_process_shares_the_session_and_holds_no_listener() {
    let inspector = HostInspector::new();
    let tree = Fixture::start(TREE_MODE);

    let own = inspector
        .process_session(std::process::id())
        .expect("read the scenario's session")
        .expect("the scenario runs");

    assert_eq!(
        inspector
            .process_session(tree.pid())
            .expect("read the image's session"),
        Some(own)
    );
    assert_eq!(
        inspector
            .holds_listener(tree.pid())
            .expect("read the image's sockets"),
        Some(false)
    );
}

#[test]
fn a_process_that_exited_has_neither_session_nor_listener() {
    let inspector = HostInspector::new();
    let mut gone = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("start a short-lived process");
    let pid = gone.id();
    gone.wait().expect("reap it");

    assert_eq!(
        inspector.process_session(pid).expect("read a gone process"),
        None
    );
    assert_eq!(
        inspector.holds_listener(pid).expect("read a gone process"),
        None
    );
}
