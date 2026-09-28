//! `pohunek service` against the real systemd user manager.
//!
//! Ignored unless `POHUNEK_SYSTEMD_E2E=1` opts in; see `support/native.rs`
//! for the isolation rules and binary lookup. The macOS counterpart is
//! `service_launchd.rs`.

#![cfg(target_os = "linux")]

// Rust guideline compliant 2026-09-24

#[path = "support/native.rs"]
mod native;

use native::Installation;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"]
async fn install_status_upgrade_refuse_and_uninstall_against_the_user_manager() {
    let installation = Installation::new();
    let namespace = installation.context.namespace().expect("namespace");
    let unit = installation
        .context
        .supervisor_dir()
        .join(namespace.daemon_unit());

    native::lifecycle(&installation).await;

    assert!(!unit.exists(), "{} is left behind", unit.display());
    assert!(!installation
        .context
        .supervisor_dir()
        .join(namespace.sessions_slice())
        .exists());
}
