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

fn discover_output(status: &str) -> std::process::Output {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\ncat <<'EOF'\n{status}\nEOF\n"),
    )
    .expect("write fake netbird");
    let inherited_path = std::env::var("PATH").expect("PATH");
    env.command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "discover", "--refresh", "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .output()
        .expect("run CLI discovery")
}

fn discover_with_status(status: &str) -> Vec<serde_json::Value> {
    let output = discover_output(status);
    assert!(
        output.status.success(),
        "discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON envelope");
    document["ok"].as_array().expect("host records").clone()
}

fn inspect_output(status: &str, host: &str) -> std::process::Output {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\ncat <<'EOF'\n{status}\nEOF\n"),
    )
    .expect("write fake netbird");
    let inherited_path = std::env::var("PATH").expect("PATH");
    env.command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "inspect", host, "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .output()
        .expect("run CLI inspect")
}

#[test]
fn inspect_rejects_unknown_and_ambiguous_netbird_hosts() {
    let unknown = inspect_output(r#"{"peers":[]}"#, "missing");
    assert!(!unknown.status.success());
    let unknown_doc: serde_json::Value =
        serde_json::from_slice(&unknown.stdout).expect("versioned JSON error");
    assert!(unknown_doc["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("missing")));

    let ambiguous = inspect_output(
        r#"{"peers":[
            {"fqdn":"build.one.example","netbirdIp":"100.64.0.2"},
            {"fqdn":"build.two.example","netbirdIp":"100.64.0.3"}
        ]}"#,
        "build",
    );
    assert!(!ambiguous.status.success());
    let ambiguous_doc: serde_json::Value =
        serde_json::from_slice(&ambiguous.stdout).expect("versioned JSON error");
    assert_eq!(ambiguous_doc["err"]["code"], "overlay_host_ambiguous");
    assert!(ambiguous_doc["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("build")));
}

#[test]
fn inspect_never_dials_an_unlisted_or_untrusted_netbird_address() {
    let known_peer = r#"{"peers":[{"fqdn":"safe.example","netbirdIp":"100.64.0.2"}]}"#;
    for selector in ["100.64.0.99", "8.8.8.8"] {
        let output = inspect_output(known_peer, selector);
        assert!(!output.status.success());
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("versioned JSON error");
        assert_eq!(document["err"]["code"], "host_unknown", "{selector}");
    }

    let spoofed = r#"{"peers":[{"fqdn":"evil.example","netbirdIp":"169.254.169.254"}]}"#;
    for selector in ["evil", "evil.example", "169.254.169.254"] {
        let output = inspect_output(spoofed, selector);
        assert!(!output.status.success());
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("versioned JSON error");
        assert_eq!(document["err"]["code"], "host_unknown", "{selector}");
    }
}

#[test]
fn malformed_netbird_status_is_a_cli_error() {
    for body in ["", "{ this is not json ]", "42", "[1, 2, 3]"] {
        let output = discover_output(body);
        assert!(
            !output.status.success(),
            "malformed status must fail: {body}"
        );
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("versioned JSON error");
        assert!(document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("netbird")));
    }
}

#[test]
fn discovery_keeps_peer_identity_but_never_routes_spoofed_addresses() {
    let records = discover_with_status(
        r#"{
            "netbirdIp":"100.64.0.1",
            "peers":[
                {"publicKey":"safe-key","fqdn":"safe.example","netbirdIp":"100.64.0.2"},
                {"fqdn":"missing.example"},
                {"publicKey":"spoofed-key","fqdn":"spoofed.example","netbirdIp":"127.0.0.1"}
            ]
        }"#,
    );
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["peer_id"], "safe-key");
    assert_eq!(records[0]["address"], "100.64.0.2");
    assert!(records[1]["peer_id"].is_null());
    assert!(records[1]["address"].is_null());
    assert_eq!(records[2]["peer_id"], "spoofed-key");
    assert!(records[2]["address"].is_null());
    assert!(records
        .iter()
        .all(|record| record["address"] != "100.64.0.1"));
}

#[test]
fn discovery_accepts_current_legacy_and_extended_netbird_status() {
    let current = discover_with_status(include_str!(
        "../../netbird/tests/fixtures/status_current.json"
    ));
    assert_eq!(current.len(), 2);
    assert_eq!(current[0]["peer_id"], "pubkey-b");
    assert_eq!(current[0]["address"], "100.92.30.40");
    assert_eq!(current[1]["peer_id"], "pubkey-c");

    let legacy = discover_with_status(include_str!(
        "../../netbird/tests/fixtures/status_legacy.json"
    ));
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0]["peer_id"], "pubkey-b-legacy");
    assert_eq!(legacy[0]["address"], "100.64.0.20");

    let extended = discover_with_status(include_str!(
        "../../netbird/tests/fixtures/status_unknown_fields.json"
    ));
    assert_eq!(extended.len(), 1);
    assert_eq!(extended[0]["name"], "host-b");
    assert_eq!(extended[0]["address"], "100.92.30.40");
}

#[test]
fn discovery_tolerates_missing_peer_address_and_empty_status() {
    let no_address = discover_with_status(include_str!(
        "../../netbird/tests/fixtures/status_peer_without_ip.json"
    ));
    assert_eq!(no_address.len(), 1);
    assert_eq!(no_address[0]["name"], "host-c");
    assert!(no_address[0]["address"].is_null());

    let empty = discover_with_status(include_str!(
        "../../netbird/tests/fixtures/status_minimal.json"
    ));
    assert!(empty.is_empty());

    assert!(discover_with_status(r#"{"netbirdIp":"100.64.0.1","peers":null}"#).is_empty());

    let offline = discover_with_status(include_str!(
        "../../netbird/tests/fixtures/status_offline.json"
    ));
    assert_eq!(offline.len(), 1);
    assert_eq!(offline[0]["name"], "host-b");
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
