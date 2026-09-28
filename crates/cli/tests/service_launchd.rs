//! `pohunek service` against real launchd in `gui/<uid>`.
//!
//! Runs in the ordinary macOS test run; a missing `gui/<uid>` domain fails
//! it. The daemon agent is defined in a temporary `LaunchAgents` directory
//! and every root is temporary; see `support/native.rs`. The Linux
//! counterpart is `service_systemd.rs`.

#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-09-24

#[path = "support/native.rs"]
mod native;

use native::Installation;

/// Exit status of `launchctl print` for a label absent from its domain.
const LAUNCHCTL_ABSENT: i32 = 113;

#[tokio::test(flavor = "multi_thread")]
async fn install_status_upgrade_refuse_and_uninstall_against_launchd() {
    let installation = Installation::new();
    let namespace = installation.context.namespace().expect("namespace");
    let daemon_label = namespace.daemon_label();
    let agent = installation
        .context
        .supervisor_dir()
        .join(format!("{daemon_label}.plist"));

    let workers = native::lifecycle(&installation).await;

    assert!(!agent.exists(), "{} is left behind", agent.display());
    assert!(
        !installation
            .context
            .paths()
            .launchd_definitions_dir()
            .exists(),
        "worker definitions are left behind"
    );
    assert!(
        !installation.context.paths().launchd_log_dir().exists(),
        "launchd logs are left behind"
    );
    assert_absent(&daemon_label);
    for worker in &workers {
        let key = pohunek_platform::supervisor::WorkerKey::from_service_id(
            &pohunek_platform::supervisor::ServiceId::parse(worker.as_str())
                .expect("worker service id"),
        )
        .expect("worker key");
        assert_absent(&namespace.worker_label(&key));
    }
}

/// Asserts that `label` is not loaded in `gui/<uid>`.
fn assert_absent(label: &str) {
    let target = format!("gui/{}/{label}", nix::unistd::Uid::effective().as_raw());
    let status = std::process::Command::new("/bin/launchctl")
        .args(["print", &target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run launchctl");
    assert_eq!(
        status.code(),
        Some(LAUNCHCTL_ABSENT),
        "{target} is still loaded"
    );
}
