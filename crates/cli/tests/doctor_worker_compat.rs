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
