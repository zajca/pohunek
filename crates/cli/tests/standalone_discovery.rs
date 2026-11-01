use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::PermissionsExt as _;

use pohunek_test_support::env::TestEnv;

#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[cfg(target_os = "linux")]
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

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

/// The peer records of a discovery run; this machine's own record is left to
/// the scenarios that look for it.
fn discover_with_status(status: &str) -> Vec<serde_json::Value> {
    discover_all_with_status(status)
        .into_iter()
        .filter(|record| record.get("local").is_none())
        .collect()
}

fn discover_all_with_status(status: &str) -> Vec<serde_json::Value> {
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
#[cfg(target_os = "linux")]
fn discovery_runs_only_a_trusted_netbird_executable_from_path() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let loose_bin = env.root().join("loose-bin");
    let safe_bin = env.root().join("safe-bin");
    fs::create_dir_all(&loose_bin).expect("create loose bin");
    fs::create_dir_all(&safe_bin).expect("create safe bin");
    let loose_marker = env.root().join("loose-ran");
    let safe_marker = env.root().join("safe-ran");
    let loose = loose_bin.join("netbird");
    let safe = safe_bin.join("netbird");
    pohunek_test_support::fs::write_executable(
        &loose,
        format!(
            "#!/bin/sh\nprintf 'ran' > '{}'\nprintf '%s\\n' '{{\"peers\":[]}}'\n",
            loose_marker.display()
        ),
    )
    .expect("write loose binary");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o777))
        .expect("make candidate world-writable");
    pohunek_test_support::fs::write_executable(
        &safe,
        format!(
            "#!/bin/sh\nprintf 'ran' > '{}'\nprintf '%s\\n' '{{\"peers\":[]}}'\n",
            safe_marker.display()
        ),
    )
    .expect("write safe binary");

    let run = |path: String| {
        env.command(pohunek_test_support::bin_exe("pohunek"))
            .args(["host", "discover", "--refresh", "--json"])
            .env("PATH", path)
            .output()
            .expect("run CLI discovery")
    };
    for path in [
        String::new(),
        "relative/bin:.".to_owned(),
        loose_bin.display().to_string(),
    ] {
        let rejected = run(path);
        assert!(!rejected.status.success());
        let rejected_doc: serde_json::Value =
            serde_json::from_slice(&rejected.stdout).expect("versioned JSON error");
        assert_eq!(rejected_doc["err"]["code"], "netbird_cli_missing");
    }
    assert!(!loose_marker.exists());

    let selected = run(format!("{}:{}", loose_bin.display(), safe_bin.display()));
    assert!(
        selected.status.success(),
        "trusted binary failed: {}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(!loose_marker.exists());
    assert_eq!(
        fs::read_to_string(safe_marker).expect("trusted binary ran"),
        "ran"
    );
}

#[test]
fn discovery_normalizes_peer_cidr_and_rejects_addresses_outside_netbird() {
    let records = discover_with_status(
        r#"{"peers":[
            {"fqdn":"lower.example","netbirdIp":"100.64.0.0/10"},
            {"fqdn":"upper.example","netbirdIp":"100.127.255.255"},
            {"fqdn":"middle.example","netbirdIp":" 100.92.10.20 "},
            {"fqdn":"below.example","netbirdIp":"100.63.255.255"},
            {"fqdn":"above.example","netbirdIp":"100.128.0.0"},
            {"fqdn":"private.example","netbirdIp":"10.0.0.1"},
            {"fqdn":"ipv6.example","netbirdIp":"::1"},
            {"fqdn":"invalid.example","netbirdIp":"not-an-ip"}
        ]}"#,
    );
    let addresses: Vec<&serde_json::Value> =
        records.iter().map(|record| &record["address"]).collect();
    assert_eq!(addresses[0], "100.64.0.0");
    assert_eq!(addresses[1], "100.127.255.255");
    assert_eq!(addresses[2], "100.92.10.20");
    assert!(addresses[3..].iter().all(|address| address.is_null()));
}

#[test]
fn discovery_bounds_unicode_errors_from_the_netbird_process() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let detail = "α".repeat(400);
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\ncat <<'EOF' >&2\n{detail}\nEOF\nexit 1\n"),
    )
    .expect("write failing NetBird fixture");
    let inherited_path = std::env::var("PATH").expect("PATH");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "discover", "--refresh", "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .output()
        .expect("run CLI discovery");
    assert!(!output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON error");
    assert_eq!(document["err"]["code"], "netbird_state_unavailable");
    let message = document["err"]["msg"].as_str().expect("error message");
    assert!(message.ends_with('…'), "error detail was not truncated");
    assert!(!message.contains('�'), "Unicode must stay valid");
    assert!(message.len() < detail.len(), "error detail must be bounded");
}

#[test]
#[cfg(target_os = "macos")]
fn discovery_uses_only_trusted_home_fallback_executables() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let local_bin = env.home().join(".local/bin");
    let cargo_bin = env.home().join(".cargo/bin");
    fs::create_dir_all(&local_bin).expect("create first fallback directory");
    fs::create_dir_all(&cargo_bin).expect("create second fallback directory");
    let local_marker = env.root().join("local-ran");
    let cargo_marker = env.root().join("cargo-ran");
    let local = local_bin.join("netbird");
    pohunek_test_support::fs::write_executable(
        &local,
        format!(
            "#!/bin/sh\nprintf 'local' >> '{}'\nprintf '%s\\n' '{{\"peers\":[]}}'\n",
            local_marker.display()
        ),
    )
    .expect("write first fallback executable");
    pohunek_test_support::fs::write_executable(
        cargo_bin.join("netbird"),
        format!(
            "#!/bin/sh\nprintf 'cargo' >> '{}'\nprintf '%s\\n' '{{\"peers\":[]}}'\n",
            cargo_marker.display()
        ),
    )
    .expect("write second fallback executable");

    let run = || {
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(["host", "discover", "--refresh", "--json"])
            .env("PATH", "")
            .output()
            .expect("run CLI discovery");
        assert!(
            output.status.success(),
            "fallback discovery failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run();
    assert_eq!(
        fs::read_to_string(&local_marker).expect("first fallback ran"),
        "local"
    );
    assert!(!cargo_marker.exists());

    fs::set_permissions(&local, fs::Permissions::from_mode(0o777))
        .expect("make first fallback executable untrusted");
    run();
    assert_eq!(
        fs::read_to_string(&local_marker).expect("first fallback marker"),
        "local"
    );
    assert_eq!(
        fs::read_to_string(&cargo_marker).expect("second fallback ran"),
        "cargo"
    );
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn discovery_creates_the_pohunek_cache_directory_owner_private() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    fake_netbird(&bin);
    let pohunek_cache = env.cache_home().join("pohunek");
    assert!(!pohunek_cache.exists(), "the scenario starts without it");
    let inherited_path = std::env::var("PATH").expect("PATH");

    // The CLI runs under the umask of a default login session, under which a
    // plain `mkdir` is 0755; `sh` sets it so this test binary keeps its own.
    let output = env
        .command("/bin/sh")
        .args(["-c", "umask 022; exec \"$0\" \"$@\""])
        .arg(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "discover", "--refresh", "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .output()
        .expect("run CLI discovery");

    assert!(
        output.status.success(),
        "discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The daemon refuses a non-private `XDG_CACHE_HOME/pohunek` as a worker
    // directory parent, so the client must not create it wider.
    for directory in [pohunek_cache.clone(), pohunek_cache.join("host-discovery")] {
        let mode = fs::metadata(&directory)
            .expect("the discovery cache directory exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{}", directory.display());
    }
}

/// This machine's own `NetBird` name and address in the local-record scenarios.
#[cfg(target_os = "linux")]
const SELF_FQDN: &str = "fixture-self.netbird.test";
#[cfg(target_os = "linux")]
const SELF_IP: &str = "100.64.0.7";

/// Runs `host discover --refresh --json` against a fake `netbird` reporting
/// `status`; every `NetBird` address is redirected to loopback `port`.
#[cfg(target_os = "linux")]
fn discover_redirected(status: &str, port: u16) -> Vec<serde_json::Value> {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\ncat <<'EOF'\n{status}\nEOF\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(env.root());
    let inherited_path = std::env::var("PATH").expect("PATH");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "discover", "--refresh", "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .output()
        .expect("run CLI discovery");
    assert!(
        output.status.success(),
        "discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON envelope");
    document["ok"].as_array().expect("host records").clone()
}

#[cfg(target_os = "linux")]
fn status_with_self(self_address: Option<&str>) -> String {
    let mut document = serde_json::json!({
        "fqdn": SELF_FQDN,
        "peers": {"details": [{
            "fqdn": "fixture-remote.netbird.test",
            "netbirdIp": "100.64.0.42",
            "publicKey": "remote-key",
            "status": "Connected"
        }]}
    });
    if let Some(address) = self_address {
        document["netbirdIp"] = serde_json::json!(address);
    }
    document.to_string()
}

#[test]
#[cfg(target_os = "linux")]
fn this_machine_is_listed_first_as_one_local_record() {
    let port = pin_overlay::serve_health();
    let records = discover_redirected(&status_with_self(Some(SELF_IP)), port);
    assert_eq!(records.len(), 2, "{records:?}");
    let locals = records
        .iter()
        .filter(|record| record.get("local").is_some())
        .count();
    assert_eq!(locals, 1, "exactly one local record: {records:?}");
    let local = &records[0];
    assert_eq!(local["local"], true);
    assert_eq!(local["name"], "fixture-self");
    assert_eq!(local["fqdn"], SELF_FQDN);
    assert_eq!(local["address"], SELF_IP);
    assert_eq!(local["port"], port);
    assert_eq!(local["overlay"], "netbird");
    assert!(local["peer_id"].is_null());
    assert_eq!(local["classification"], "reachable_daemon");
    assert_eq!(local["daemon_version"], "fixture");
    let peer = &records[1];
    assert_eq!(peer["peer_id"], "remote-key");
    assert!(
        peer.get("local").is_none(),
        "a peer record is unchanged on the wire: {peer}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn a_failed_probe_of_this_machine_reads_unreachable() {
    let records = discover_redirected(
        &status_with_self(Some(SELF_IP)),
        pin_overlay::serve_unresponsive(),
    );
    let local = records
        .iter()
        .find(|record| record.get("local").is_some())
        .expect("the local record");
    assert_eq!(
        local["classification"], "unreachable",
        "a failed self-probe is never a healthy default: {local}"
    );
    assert!(
        local["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the record says why the probe failed: {local}"
    );
    let reachable = discover_redirected(
        &status_with_self(Some(SELF_IP)),
        pin_overlay::serve_health(),
    );
    assert!(
        reachable
            .iter()
            .all(|record| record.get("reason").is_none()),
        "no record carries a reason when the probe succeeds: {reachable:?}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn this_machine_without_an_address_is_a_candidate() {
    let records = discover_redirected(&status_with_self(None), pin_overlay::serve_unresponsive());
    let local = &records[0];
    assert_eq!(local["local"], true);
    assert!(local["address"].is_null());
    assert_eq!(local["classification"], "candidate");
}

#[test]
fn no_local_record_without_an_own_identity() {
    let records = discover_all_with_status(
        r#"{"peers":[{"publicKey":"safe-key","fqdn":"safe.example","netbirdIp":"100.64.0.2"}]}"#,
    );
    assert_eq!(records.len(), 1);
    assert!(records.iter().all(|record| record.get("local").is_none()));
}

#[test]
#[cfg(target_os = "linux")]
fn a_cache_of_the_previous_schema_is_discarded_not_read() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\ncat <<'EOF'\n{}\nEOF\n",
            r#"{"peers":[{"publicKey":"fresh-key","fqdn":"fresh.example","netbirdIp":"100.64.0.2"}]}"#
        ),
    )
    .expect("write fake netbird");
    let inherited_path = std::env::var("PATH").expect("PATH");
    let run = || {
        env.command(pohunek_test_support::bin_exe("pohunek"))
            .args(["host", "list", "--json"])
            .env("PATH", format!("{}:{inherited_path}", bin.display()))
            .output()
            .expect("run CLI")
    };
    // A current cache is written first, then its schema number is set back to
    // the previous revision, as an older CLI left it.
    assert!(run().status.success());
    let cache = env.cache_home().join("pohunek/host-discovery/records.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).expect("read the cache")).expect("cache JSON");
    snapshot["schema"] = serde_json::json!(3);
    snapshot["records"] = serde_json::json!([{
        "name": "stale", "fqdn": null, "address": null, "port": 1,
        "overlay": "netbird", "peer_id": "stale-key", "classification": "candidate"
    }]);
    fs::write(&cache, snapshot.to_string()).expect("write the old cache");
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o600)).expect("private cache");

    let output = run();
    assert!(output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON envelope");
    let names: Vec<&str> = document["ok"]
        .as_array()
        .expect("records")
        .iter()
        .filter_map(|record| record["peer_id"].as_str())
        .collect();
    assert_eq!(
        names,
        ["fresh-key"],
        "the old cache is refreshed: {document}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn this_machine_and_its_peers_come_from_one_netbird_run() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let runs = env.root().join("netbird-runs");
    // The first run answers with this machine and a peer; any later run would
    // answer with a machine of another name and no peer.
    let later =
        serde_json::json!({"fqdn": "renamed.netbird.test", "netbirdIp": SELF_IP, "peers": []});
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\nprintf x >> '{runs}'\nif [ \"$(wc -c < '{runs}')\" -gt 1 ]; then\ncat <<'EOF'\n{later}\nEOF\nexit 0\nfi\ncat <<'EOF'\n{first}\nEOF\n",
            runs = runs.display(),
            first = status_with_self(Some(SELF_IP)),
        ),
    )
    .expect("write fake netbird");
    let port = pin_overlay::serve_health();
    let (library, capture) = connect_redirect::build(env.root());
    let inherited_path = std::env::var("PATH").expect("PATH");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "discover", "--refresh", "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .output()
        .expect("run CLI discovery");
    assert!(
        output.status.success(),
        "discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&runs).expect("netbird ran"),
        "x",
        "one netbird run serves the peers and this machine's own identity"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON envelope");
    let records = document["ok"].as_array().expect("host records");
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["local"], true);
    assert_eq!(records[0]["fqdn"], SELF_FQDN);
    assert_eq!(records[1]["peer_id"], "remote-key");
}

/// A `netbird status` that never answers ends at the configured deadline as
/// `netbird_state_unavailable`, and the CLI it started is gone.
#[cfg(target_os = "linux")]
#[test]
fn a_netbird_probe_that_never_answers_is_bounded_and_killed() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let dir = env.config_home().join("pohunek");
    fs::create_dir_all(&dir).expect("create the config directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = dir.join("overlays.toml");
    fs::write(
        &file,
        "providers = [\"netbird\"]\nstatus_timeout_secs = 1\n",
    )
    .expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let pid = env.root().join("netbird.pid");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 3600\n",
            pid.display()
        ),
    )
    .expect("write the hanging fake netbird");

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["host", "discover", "--refresh", "--json"])
        .env("PATH", &bin)
        .output()
        .expect("run CLI discovery");
    assert!(!output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("versioned JSON error");
    assert_eq!(document["err"]["code"], "netbird_state_unavailable");
    assert!(document["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("did not answer within")));
    let pid = fs::read_to_string(&pid).expect("the fake recorded its pid");
    assert!(
        !std::path::Path::new("/proc").join(pid.trim()).exists(),
        "the hung CLI was killed and reaped"
    );
}

/// A `NetBird` address that two peers list is no route: the listing keeps both
/// peers as candidates and names why, the selectors that reach it are
/// ambiguous, and a peer with its own address is unaffected.
#[test]
fn an_address_listed_for_two_netbird_peers_is_not_used_for_routing() {
    let status = r#"{"peers":{"details":[
        {"fqdn":"one.example","netbirdIp":"100.64.0.2","publicKey":"key-one","status":"Connected"},
        {"fqdn":"two.example","netbirdIp":"100.64.0.2","publicKey":"key-two","status":"Connected"},
        {"fqdn":"solo.example","netbirdIp":"100.64.0.3","publicKey":"key-solo","status":"Connected"},
        {"fqdn":"echo.example","netbirdIp":"100.64.0.7","publicKey":"key-echo","status":"Connected"}
    ]},"netbirdIp":"100.64.0.7","fqdn":"self.example"}"#;
    let peers = discover_with_status(status);
    for name in ["one", "two", "echo"] {
        let entry = peers
            .iter()
            .find(|peer| peer["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed: {peers:?}"));
        assert!(entry["address"].is_null(), "{name}: {entry}");
        assert_eq!(entry["classification"], "candidate", "{name}");
        assert_eq!(entry["reason"], "address_ambiguous", "{name}");
    }
    let solo = peers
        .iter()
        .find(|peer| peer["name"] == "solo")
        .expect("solo is listed");
    assert_eq!(solo["address"], "100.64.0.3");
    assert!(solo["reason"].is_null(), "{solo}");

    let expected = [
        ("one", vec!["peer one.example", "peer two.example"]),
        ("two.example", vec!["peer one.example", "peer two.example"]),
        ("100.64.0.2", vec!["peer one.example", "peer two.example"]),
        (
            "echo",
            vec!["this machine (self.example)", "peer echo.example"],
        ),
    ];
    for (selector, candidates) in expected {
        let output = inspect_output(status, selector);
        assert!(!output.status.success(), "{selector}");
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("versioned JSON error");
        assert_eq!(
            document["err"]["code"], "overlay_host_ambiguous",
            "{selector}"
        );
        let message = document["err"]["msg"].as_str().expect("error message");
        for candidate in candidates {
            assert!(
                message.contains(candidate),
                "{selector}: {candidate} is named in {message}"
            );
        }
    }
    #[cfg(target_os = "linux")]
    for selector in ["one", "two.example", "echo"] {
        let (output, dialed) = dial_redirected(status, selector);
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("versioned JSON error");
        assert_eq!(
            document["err"]["code"], "overlay_host_ambiguous",
            "{selector}"
        );
        assert!(dialed.is_empty(), "{selector} dialed {dialed:?}");
    }
}

/// Runs `--host <selector> session list --json` against a fake `netbird`
/// reporting `status`, and returns the output with the addresses the run
/// connected to.
#[cfg(target_os = "linux")]
fn dial_redirected(status: &str, selector: &str) -> (std::process::Output, Vec<String>) {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let bin = env.root().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\ncat <<'EOF'\n{status}\nEOF\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(env.root());
    let inherited_path = std::env::var("PATH").expect("PATH");
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["--host", selector, "session", "list", "--json"])
        .env("PATH", format!("{}:{inherited_path}", bin.display()))
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .output()
        .expect("run CLI dial");
    let dialed = fs::read_to_string(&capture)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (output, dialed)
}
