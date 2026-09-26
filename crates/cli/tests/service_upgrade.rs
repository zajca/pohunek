//! `pohunek service upgrade` with live workers against the real native manager.
//!
//! One build has one version, so the test installs this build as the real
//! version A and upgrades to version B, a second directory holding the same
//! build. The engine's `test-util` override lets the staged binaries report A
//! while they are installed as B; staging, the transaction journal,
//! `service.toml`, the daemon replacement, readiness, and version GC all run
//! unchanged.
//!
//! Linux drives the systemd user manager and is ignored unless
//! `POHUNEK_SYSTEMD_E2E=1` opts in; macOS drives `gui/<uid>` in the ordinary
//! test run. See `support/native.rs` for isolation and binary lookup.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-09-24

#[path = "support/native.rs"]
mod native;

use std::path::PathBuf;

use native::{connect, eventually, Installation};
use pohunek_cli::service::{Backend, Engine, VERSION};
use pohunek_platform::process::{HostInspector, ProcessIdentity, ProcessInspector as _};
use pohunek_platform::supervisor::ServiceObservation;

/// Version directory the upgrade installs this build under.
const UPGRADE_SUFFIX: &str = "-upgrade";

/// Live identity of one worker generation and its PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Runtime {
    job: String,
    worker: ProcessIdentity,
    executable: PathBuf,
    child: ProcessIdentity,
    tty: String,
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn upgrade_with_live_workers_keeps_their_pty_and_version() {
    let mut installation = Installation::new();
    // The upgraded daemon runs this build, which reports the real version
    // rather than the upgrade's directory name; teardown must accept it.
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let prefix = installation.root.join("prefix");
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let versions = prefix.join("libexec/pohunek");

    Engine::new(&installation.context, &backend)
        .install(&from, &prefix, &old)
        .await
        .expect("install version A");
    let old_daemon = daemon_process(&backend).await;
    let sessions = [
        installation.new_session("upgrade-a").await,
        installation.new_session("upgrade-b").await,
    ];
    let before = runtimes(&installation, &backend, &sessions).await;
    for runtime in &before {
        assert_eq!(
            runtime.executable,
            versions.join(&old).join("pohunek-sessiond")
        );
    }

    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(old.clone())
        .upgrade(&from, &new)
        .await
        .expect("upgrade to version B");
    assert_eq!(
        (report.from_version.as_str(), report.to_version.as_str()),
        (old.as_str(), new.as_str())
    );
    assert!(!report.unchanged);
    assert_eq!(report.gc_error, None);
    assert!(
        report.kept_versions.iter().any(|kept| kept.version == old),
        "GC keeps the version live workers run: {report:?}"
    );
    assert!(!report.removed_versions.contains(&old));
    assert!(versions.join(&old).join("pohunek-sessiond").is_file());

    // Both directories hold one build, so readiness cannot tell the daemons
    // apart; the restart is observed through the job and the process image.
    let daemon_executable = versions.join(&new).join("pohunekd");
    let new_daemon = eventually("the daemon restarted from version B", || async {
        let daemon = backend.daemon().inspect().await.ok()?;
        let process = daemon.process.filter(|process| *process != old_daemon)?;
        (daemon
            .definition
            .as_ref()
            .map(|facts| facts.executable.as_path())
            == Some(daemon_executable.as_path())
            && executable_of(process.pid).as_deref() == Some(daemon_executable.as_path()))
        .then_some(process)
    })
    .await;
    installation.client().await;
    eprintln!("daemon {} -> {}", old_daemon.pid, new_daemon.pid);

    let after = runtimes(&installation, &backend, &sessions).await;
    assert_eq!(after, before, "workers kept PID, child, PTY, and version A");

    let fresh = installation.new_session("upgrade-c").await;
    let fresh = runtimes(&installation, &backend, &[fresh]).await;
    assert_eq!(
        fresh[0].executable,
        versions.join(&new).join("pohunek-sessiond")
    );
}

/// Captures each session's worker job, process, executable, and PTY.
async fn runtimes(
    installation: &Installation,
    backend: &Backend,
    sessions: &[String],
) -> Vec<Runtime> {
    let inspector = HostInspector::new();
    let jobs = backend
        .workers()
        .discover()
        .await
        .expect("discover workers");
    let mut runtimes = Vec::new();
    for session in sessions {
        let info = installation.live(session).await;
        let matching = jobs
            .iter()
            .filter(|job| job.id.as_str().starts_with(&format!("{session}.")))
            .collect::<Vec<&ServiceObservation>>();
        let [job] = matching.as_slice() else {
            panic!("{session} has exactly one worker job: {matching:?}");
        };
        runtimes.push(Runtime {
            job: job.id.to_string(),
            worker: job.process.expect("the worker job has a process"),
            executable: job
                .definition
                .as_ref()
                .expect("the worker job has a definition")
                .executable
                .clone(),
            child: inspector
                .identity(info.pid)
                .expect("inspect the PTY child")
                .expect("the PTY child runs"),
            tty: tty(info.pid),
        });
    }
    runtimes
}

async fn daemon_process(backend: &Backend) -> ProcessIdentity {
    eventually("daemon process", || async {
        backend
            .daemon()
            .inspect()
            .await
            .ok()
            .and_then(|job| job.process)
    })
    .await
}

fn executable_of(pid: u32) -> Option<PathBuf> {
    HostInspector::new()
        .executable(pid)
        .expect("inspect the daemon executable")
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
}

fn tty(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8(output.stdout)
        .expect("ps output is UTF-8")
        .trim()
        .to_owned()
}
