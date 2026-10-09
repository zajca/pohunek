use std::fs;

use pohunek_test_support::env::TestEnv;

fn fake_netbird(bin: &std::path::Path) {
    let path = bin.join("netbird");
    pohunek_test_support::fs::write_executable(
        &path,
        "#!/bin/sh\nprintf '%s\\n' '{\"peers\":[]}'\n",
    )
    .expect("write fake netbird");
}

#[test]
fn discover_and_list_json_need_cache_and_netbird_but_not_runtime_socket() {
    // The environment's canonical root keeps the CLI's trusted cache-directory
    // checks clear of a symlinked ancestor (macOS `/var`).
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    fake_netbird(&bin);
    let inherited_path = std::env::var("PATH").expect("PATH");
    let path = format!("{}:{inherited_path}", bin.display());

    for command in ["discover", "list"] {
        // The scrubbed environment carries no origin pair of a developer's own
        // pohunek session; discovery must work without HOME and a runtime
        // directory.
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(["host", command, "--json"])
            .env("PATH", &path)
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("HOME")
            .output()
            .expect("run CLI");
        assert!(
            output.status.success(),
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("versioned JSON envelope");
        assert_eq!(document["ok"], serde_json::json!([]));
        assert!(document["cli_version"].is_string());
        assert!(document["protocol"]["maximum"].is_number());
    }
}

#[test]
fn local_health_rejects_an_empty_runtime_base_before_connecting() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["health", "--json"])
        .env("XDG_RUNTIME_DIR", "")
        .output()
        .expect("run CLI");
    assert!(!output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON error envelope");
    assert_eq!(document["err"]["class"], "configuration");
    assert_eq!(document["err"]["code"], "paths_unavailable");
    assert!(document["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("XDG_RUNTIME_DIR must not be empty")));
}

#[cfg(target_os = "linux")]
#[test]
fn local_health_rejects_a_missing_runtime_base_before_connecting() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["health", "--json"])
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("run CLI");
    assert!(!output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON error envelope");
    assert_eq!(document["err"]["class"], "configuration");
    assert_eq!(document["err"]["code"], "missing_env");
    assert!(document["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("XDG_RUNTIME_DIR is not set")));
}
