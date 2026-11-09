//! `pohunek service check|upgrade|uninstall` against units the removed
//! workflow plugin feature left in the service manager's unit directory.
//!
//! No service manager is involved: the refusal comes before any connection,
//! from the files in the unit directory.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-11-09

use pohunek_cli::service::Context;
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_test_support::env::TestEnv;

/// The file name of a workflow unit of `namespace` on this platform.
fn unit_name(namespace: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("io.github.zajca.pohunek.{namespace}.wf.0123456789ab.j.nightly.plist")
    } else {
        format!("pohunek-{namespace}-wf-0123456789ab-j-nightly.service")
    }
}

#[test]
fn a_leftover_workflow_unit_stops_check_upgrade_and_uninstall_with_its_name() {
    let env = TestEnv::new().expect("hermetic environment");
    let paths = BasePaths::resolve_for(
        Platform::current().expect("platform"),
        nix::unistd::Uid::effective().as_raw(),
        &PathEnv {
            xdg_runtime_dir: Some(env.runtime_dir().into()),
            xdg_data_home: Some(env.data_home().into()),
            xdg_state_home: Some(env.state_home().into()),
            xdg_cache_home: Some(env.cache_home().into()),
            xdg_config_home: Some(env.config_home().into()),
            home: Some(env.home().into()),
        },
    )
    .expect("resolve the hermetic paths");
    let supervisor_dir =
        pohunek_cli::service::context::default_supervisor_dir(&paths, Some(env.home()))
            .expect("unit directory");
    let namespace = Context::new(
        paths,
        nix::unistd::Uid::effective().as_raw(),
        Some(env.home().to_path_buf()),
        Some(env.runtime_dir().to_path_buf()),
        supervisor_dir.clone(),
        pohunek_test_support::bin_exe("pohunek"),
    )
    .namespace_without_creating()
    .expect("namespace");

    std::fs::create_dir_all(&supervisor_dir).expect("unit directory");
    let unit = unit_name(namespace.as_str());
    std::fs::write(supervisor_dir.join(&unit), "[Service]\n").expect("write the leftover unit");

    for arguments in [
        &["service", "check"][..],
        &["service", "upgrade"][..],
        &["service", "uninstall"][..],
    ] {
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(arguments)
            .arg("--json")
            .output()
            .expect("run pohunek");
        assert!(!output.status.success(), "{arguments:?}");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("service_workflow_units_installed") && text.contains(&unit),
            "{arguments:?}: {text}"
        );
    }
}
