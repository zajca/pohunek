//! Supported-boundary coverage of the daemon worker probe through CLI doctor.

// Rust guideline compliant 2026-10-11

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use pohunek_test_support::fs::write_executable;
use pohunek_test_support::worker_binary;
use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, Harness};

const WORKER_CANARY: &str = "WORKER_COMPAT_SECRET_CANARY";

fn check<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["ok"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("doctor contains {name}: {document}"))
}

async fn assert_worker_failure(harness: &Harness, worker: &std::path::Path, detail: &str) {
    let json = harness.run(&["doctor", "--json"]).await;
    assert_eq!(json.status.code(), Some(1), "JSON doctor must fail");
    let document: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    assert_eq!(document["ok"]["overall"], "fail");
    let worker_check = check(&document, "worker_executable");
    assert_eq!(worker_check["status"], "fail");
    assert!(worker_check["detail"]
        .as_str()
        .unwrap_or_default()
        .contains(detail));
    let profile = check(&document, "agent_profile_broken");
    assert_eq!(profile["status"], "fail");
    assert!(!json
        .stdout
        .windows(worker.as_os_str().as_encoded_bytes().len())
        .any(|window| window == worker.as_os_str().as_encoded_bytes()));
    assert!(!json
        .stdout
        .windows(WORKER_CANARY.len())
        .any(|window| window == WORKER_CANARY.as_bytes()));

    let human = harness.run(&["doctor"]).await;
    assert_eq!(human.status.code(), Some(1), "human doctor must fail");
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("worker_executable"));
    assert!(human.contains("agent_profile_broken"));
    assert!(human.contains(detail));
    assert!(human.contains("overall: fail"));
    assert!(!human.contains(&worker.display().to_string()));
    assert!(!human.contains(WORKER_CANARY));
}

#[tokio::test]
async fn doctor_reports_missing_and_incompatible_selected_worker_through_cli() {
    let harness_root = pohunek_test_support::tempdir().expect("create worker source root");
    let selected_worker = harness_root.path().join("selected-sessiond");
    fs::copy(worker_binary(), &selected_worker).expect("copy the real session worker");
    fs::set_permissions(&selected_worker, fs::Permissions::from_mode(0o700))
        .expect("make copied worker executable private");

    let harness = Harness::start_with_worker(selected_worker.clone()).await;
    let (code, session) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            "shell",
            "--cwd",
            path_str(harness.env.cwd()),
        ])
        .await;
    assert_eq!(
        code, 0,
        "start a session with the selected real worker: {session}"
    );
    assert!(
        session["ok"]["runtime"]["worker_id"].is_string(),
        "{session}"
    );
    harness.profile("broken", "not = [valid");

    let backup = harness.env.root().join("selected-sessiond.backup");
    fs::rename(&selected_worker, &backup).expect("remove selected worker executable");
    assert_worker_failure(&harness, &selected_worker, "worker is missing or unsafe").await;
    fs::rename(&backup, &selected_worker).expect("restore selected worker executable");

    let incompatible = harness.env.root().join("incompatible-sessiond");
    write_executable(
        &incompatible,
        format!("#!/bin/sh\nprintf '%s\\n' '{WORKER_CANARY}' >&2\nexit 71\n"),
    )
    .expect("write an incompatible worker probe");
    fs::rename(&selected_worker, &backup).expect("move selected worker out of the path");
    fs::rename(&incompatible, &selected_worker)
        .expect("install incompatible probe at selected path");
    assert_worker_failure(&harness, &selected_worker, "malformed or incompatible").await;
    fs::remove_file(&selected_worker).expect("remove incompatible worker probe");
    fs::rename(&backup, &selected_worker).expect("restore selected real worker");
    harness.stop().await;
}

const SERVICE_TOML: &str = r#"schema_version = 7
prefix = "/home/u/.local"
active_version = "0.35.2"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = EXIT_TIMEOUT_MS
daemon_restart_throttle_ms = 5000

[environment]
search_path = []
allowlist = ["PATH"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 5000
submit_delay_ms = 150
write_stall_timeout_ms = 3000

[waits]
turn_default_ms = 300000
turn_max_ms = 1800000

[hooks]
stderr_tail_bytes = 4096

[events]
max_file_bytes = 33554432
max_files = 8
max_age_days = 14

[limits]
open_files = 8192
"#;

fn install_service_toml(harness: &Harness, exit_timeout_ms: u32) {
    let dir = harness.env.config_home().join("pohunek");
    fs::create_dir_all(&dir).expect("create the config directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod config directory");
    let file = dir.join("service.toml");
    fs::write(
        &file,
        SERVICE_TOML.replace("EXIT_TIMEOUT_MS", &exit_timeout_ms.to_string()),
    )
    .expect("write service.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod service.toml");
}

#[tokio::test]
async fn doctor_warns_when_shutdown_bounds_exceed_the_exit_timeout_through_cli() {
    let harness = Harness::start().await;

    install_service_toml(&harness, 30_000);
    let json = harness.run(&["doctor", "--json"]).await;
    let document: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    assert_eq!(
        check(&document, "service_config_shutdown_budget")["status"],
        "ok"
    );

    install_service_toml(&harness, 20_000);
    let json = harness.run(&["doctor", "--json"]).await;
    let document: Value = serde_json::from_slice(&json.stdout).expect("doctor JSON");
    let budget = check(&document, "service_config_shutdown_budget");
    assert_eq!(budget["status"], "warn", "{document}");
    assert!(
        budget["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("daemon_exit_timeout_ms = 20000"),
        "{budget}"
    );
    harness.stop().await;
}
