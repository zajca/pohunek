//! `pohunek assistant` installs the missing assistant package and prints a
//! runnable hint.
//!
//! A real `pohunekd` with an index trust anchor serves the installation, a
//! loopback server serves the signed schema 2 plugin index and the package
//! archive, and the CLI runs `assistant --print-prompt`, which walks the same
//! `assistant.materialize` path as a launch without needing a capable agent.

// Rust guideline compliant 2026-11-22

#![cfg(unix)]

use package::IndexChannel;
use pohunek_test_support::catalog::{anchor_for, key_id, test_key};
use serde_json::Value;

use super::plugin_catalog_install::{Anchor, Channel, Install};

#[expect(
    dead_code,
    reason = "the module is shared with scenarios that use more of its helpers"
)]
#[path = "support/assistant_index.rs"]
mod assistant_index;
// Linux-only: the fake remote daemon in S4 reaches `127.0.0.1` through
// the `connect_redirect` interposer, which relies on `LD_PRELOAD`.
// Without the gate the darwin target would keep the modules' helpers
// alive with no caller and fail `dead_code` under `-D warnings`.
#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[cfg(target_os = "linux")]
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

use assistant_index::{Archive, Keys, Server, ASSISTANT_PACKAGE, INDEX_PATH};

/// A daemon with an index trust anchor, the signing keys and a loopback
/// server that serves the signed index.
struct Rig {
    install: Install,
    server: Server,
    keys: Keys,
}

impl Rig {
    async fn start() -> Self {
        let root = test_key();
        let anchor = Anchor::File {
            bytes: anchor_for(&root),
            mode: 0o644,
        };
        let channel = IndexChannel::new(key_id(&root))
            .to_bytes()
            .expect("encode the index channel");
        let install = Install::start_with_channel(
            &anchor,
            Some(Channel {
                bytes: &channel,
                mode: 0o644,
            }),
        )
        .await;
        Self {
            install,
            server: Server::start(),
            keys: Keys::with_root(root),
        }
    }

    fn url(&self) -> String {
        self.server.url(INDEX_PATH)
    }

    /// Serves a signed index that lists `version` of the assistant package,
    /// and its archive.
    fn publish(&self, version: &str) -> Archive {
        let archive = assistant_index::assistant_archive(version, version);
        let path = assistant_index::archive_path(version);
        let entry = assistant_index::assistant_entry(&archive, version, self.server.url(&path));
        self.server.set(INDEX_PATH, self.keys.signed(vec![entry]));
        self.server.set(&path, archive.bytes.clone());
        archive
    }

    /// Serves a signed index that lists nothing.
    fn publish_nothing(&self) {
        self.server.set(INDEX_PATH, self.keys.signed(Vec::new()));
    }

    /// Runs `assistant` with `arguments` and returns the exit code, the JSON
    /// document on stdout and the human text on stderr.
    ///
    /// The launch names an explicit agent the daemon does not list, which the
    /// launcher allows without a capable runtime: `--print-prompt` never
    /// starts a session, so the agent needs no binary, and the run still walks
    /// the same `assistant.materialize` path as a launch.
    fn assistant(&self, arguments: &[&str]) -> (i32, Value, String) {
        let url = self.url();
        let mut full: Vec<&str> = vec![
            "assistant",
            "--agent",
            "fixture-agent",
            "--print-prompt",
            "--no-snapshot",
            "--no-start-daemon",
        ];
        full.extend(arguments.iter().copied());
        full.push("--json");
        let output = self
            .install
            .env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(&full)
            .env("POHUNEK_TEST_OFFICIAL_INDEX_URL_V2", &url)
            .output()
            .expect("run pohunek assistant");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {full:?} is one JSON document ({error}): {} / {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (
            output.status.code().expect("exit code"),
            document,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// The assistant package versions the daemon lists.
    async fn listed(&self) -> Vec<Value> {
        let (_code, document) = self.install.json(&["plugin", "list"]).await;
        document["ok"]["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .filter(|info| info["package"]["id"] == ASSISTANT_PACKAGE)
            .cloned()
            .collect()
    }
}

/// The daemon version the fixture daemon reports.
fn daemon_version(rig: &Rig) -> String {
    let output = rig
        .install
        .env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["daemon", "--version"])
        .output()
        .expect("run daemon --version");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[tokio::test]
async fn s1_missing_package_installs_from_the_index_and_launches() {
    let rig = Rig::start().await;
    let archive = rig.publish("0.42.0");
    let (code, document, stderr) = rig.assistant(&["--yes", "help me"]);
    assert_eq!(code, 0, "{document} / {stderr}");
    assert_eq!(
        document["ok"]["intent"], "help",
        "the launch prints the prompt: {document}"
    );
    let listed = rig.listed().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["selected"], true, "{listed:?}");
    assert_eq!(listed[0]["digest"], archive.digest.as_str(), "{listed:?}");
    assert!(!stderr.contains('<'), "{stderr}");
}

#[tokio::test]
async fn s2_without_yes_and_without_tty_nothing_installs() {
    let rig = Rig::start().await;
    rig.publish("0.42.0");
    let (code, document, stderr) = rig.assistant(&["help me"]);
    assert_ne!(
        code, 0,
        "no TTY and no --yes installs nothing: {document} / {stderr}"
    );
    assert!(rig.listed().await.is_empty(), "nothing installed");
    let combined = format!("{document}{stderr}");
    assert!(
        combined.contains("preview") || combined.contains("install"),
        "the preview is printed: {combined}"
    );
}

#[tokio::test]
async fn s3_index_without_the_package_names_the_daemon_version() {
    let rig = Rig::start().await;
    rig.publish_nothing();
    let version = daemon_version(&rig);
    let (code, document, stderr) = rig.assistant(&["--yes", "help me"]);
    assert_ne!(code, 0, "{document} / {stderr}");
    let combined = format!("{document}{stderr}");
    assert_eq!(
        document["err"]["code"], "assistant_package_missing",
        "{combined}"
    );
    assert!(
        combined.contains(&format!("not available for core v{version}")),
        "the message names the daemon version: {combined}"
    );
    assert!(
        combined.contains("pohunek assistant setup"),
        "the message names the runnable command: {combined}"
    );
    assert!(
        document["err"]["recover"]
            .as_str()
            .is_some_and(|recover| recover.contains("pohunek assistant setup")),
        "the JSON error carries the runnable remedy, not the stale daemon one: {combined}"
    );
    assert!(
        !document["err"]["recover"]
            .as_str()
            .is_some_and(|recover| recover.contains('<')),
        "no placeholder remains in the JSON recover: {combined}"
    );
    assert!(
        !combined.contains('<'),
        "no placeholder remains: {combined}"
    );
    assert!(rig.listed().await.is_empty(), "nothing installed");
}

#[tokio::test]
async fn s6_index_for_another_core_names_the_daemon_version() {
    let rig = Rig::start().await;
    // The index lists the package, but no entry supports this daemon's core:
    // the core text is checked, not assumed.
    rig.publish("9.9.9");
    let archive = assistant_index::assistant_archive("9.9.9", "9.9.9");
    let mut entry =
        assistant_index::assistant_entry(&archive, "9.9.9", rig.server.url("/9.9.9.tar.zst"));
    entry.core = "^9.0.0".to_owned();
    rig.server.set(INDEX_PATH, rig.keys.signed(vec![entry]));
    let version = daemon_version(&rig);
    let (code, document, stderr) = rig.assistant(&["--yes", "help me"]);
    assert_ne!(code, 0, "{document} / {stderr}");
    let combined = format!("{document}{stderr}");
    assert!(
        combined.contains(&format!("not available for core v{version}")),
        "the core gap names the daemon version: {combined}"
    );
    assert!(
        !combined.contains('<'),
        "no placeholder remains: {combined}"
    );
    assert!(rig.listed().await.is_empty(), "nothing installed");
}

#[tokio::test]
async fn s3b_installed_but_disabled_package_installs_nothing() {
    let rig = Rig::start().await;
    rig.publish("0.42.0");
    // The same bytes the index lists, so the manual install matches the
    // published digest and the plan sees an installed but disabled package.
    let archive = assistant_index::assistant_archive("0.42.0", "0.42.0");
    let path = rig
        .install
        .write("assistant-disabled.tar.zst", &archive.bytes);
    let hex = archive
        .digest
        .as_str()
        .trim_start_matches("sha256:")
        .to_owned();
    let (code, document) = rig
        .install
        .json(&[
            "plugin",
            "install",
            path.to_str().expect("UTF-8 path"),
            "--sha256",
            &hex,
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    let (code, document) = rig
        .install
        .json(&["plugin", "disable", ASSISTANT_PACKAGE])
        .await;
    assert_eq!(code, 0, "{document}");
    let (code, document, stderr) = rig.assistant(&["--yes", "help me"]);
    assert_ne!(code, 0, "{document} / {stderr}");
    let combined = format!("{document}{stderr}");
    assert!(
        combined.contains("disabled") || combined.contains("not enabled"),
        "the plan outcome is printed: {combined}"
    );
}

/// A fixture remote daemon behind the overlay redirect: `host.inspect`
/// answers a minimal capability document and `assistant.materialize`
/// refuses with the historical N-1 placeholder remedy, so the remote
/// recovery branch is reached through a real connection.
///
/// Linux-only with its only caller S4: the darwin target must not keep
/// a helper no test there can reach.
#[cfg(target_os = "linux")]
fn serve_remote_missing_package(
    listener: std::net::TcpListener,
    served: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = std::sync::Arc::clone(&served);
            std::thread::spawn(move || serve_remote_connection(stream, &served));
        }
    });
}

/// Linux-only with `serve_remote_missing_package`, its only caller.
#[cfg(target_os = "linux")]
fn serve_remote_connection(
    stream: std::net::TcpStream,
    served: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    use std::io::{BufRead as _, BufReader, Write as _};
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let request: protocol::Request =
            serde_json::from_str(line.trim_end()).expect("parse a request");
        served
            .lock()
            .expect("served requests")
            .push(request.method().to_owned());
        let response = if request.method() == protocol::method::ASSISTANT_MATERIALIZE {
            // The historical N-1 remedy, with placeholders the recovery
            // must replace before anything reaches the output.
            protocol::Response::err(
                protocol::PROTOCOL_VERSION,
                request.id(),
                protocol::ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    "assistant_package_missing",
                    "the assistant context package `pohunek.context.assistant` cannot be used: it is not installed".to_owned(),
                    Some(
                        "install it with `pohunek plugin install <archive> --sha256 <digest>`"
                            .to_owned(),
                    ),
                ),
            )
            .expect("valid fixture response")
        } else if request.method() == protocol::method::HOST_INSPECT {
            protocol::Response::ok(
                protocol::PROTOCOL_VERSION,
                request.id(),
                serde_json::json!({
                    "daemon_version": "0.42.0",
                    "protocol_version": protocol::PROTOCOL_VERSION.get(),
                    "supported_agents": ["codex"],
                    "runtimes": [{"agent": "codex", "available": true, "supported": true}],
                    "git_available": true,
                    "worktree_supported": true,
                    "terminal_read_supported": true,
                    "output_read_supported": true,
                    "session_wait_supported": false,
                    "session_wait_turn_supported": false,
                    "session_new_cwd_supported": true,
                    "session_new_input_ack_supported": false,
                    "session_new_secret_env_supported": false,
                    "session_new_request_id_supported": false,
                    "session_new_context_pin_supported": false,
                    "session_new_fetch_ref_supported": false,
                    "session_attention_isolation_supported": false,
                    "session_list_tree_filters_supported": false,
                    "session_retention_selector_supported": false,
                    "session_remove_tree_supported": false,
                    "session_adopt_supported": false,
                    "package_install_commands_supported": false,
                    "package_install_index_supported": false,
                    "package_bind_profile_expected_supported": false,
                    "package_install_index_context_supported": false,
                    "package_create_profile_supported": false,
                    "package_create_profile_options_supported": false,
                    "package_create_preset_supported": false,
                    "package_remove_profile_supported": false,
                    "integration_agent_skill_supported": false,
                    "package_index_apply_supported": false,
                    "session_new_branch_source_supported": false,
                    "session_new_agent_options_supported": false,
                    "session_new_skip_repo_hooks_supported": false,
                }),
            )
            .expect("valid fixture response")
        } else if request.method() == protocol::method::DAEMON_HEALTH {
            protocol::Response::ok(
                protocol::PROTOCOL_VERSION,
                request.id(),
                serde_json::json!({
                    "status": "ok",
                    "daemon_version": "0.42.0",
                    "protocol_version": protocol::PROTOCOL_VERSION.get()
                }),
            )
            .expect("valid fixture response")
        } else {
            panic!(
                "the remote assistant sent an unexpected request: {}",
                request.method()
            );
        };
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        )
        .expect("write the fixture response");
    }
}

/// A remote `assistant_package_missing` error keeps its code and names the
/// runnable command for that host: nothing is installed remotely, because
/// `package install` is local-only.
///
/// Linux-only: the fixture reaches the fake remote daemon through the
/// `connect_redirect` interposer, which relies on `LD_PRELOAD`. macOS has no
/// `LD_PRELOAD`, so the dial would time out with `host_unreachable` there.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn s4_remote_host_installs_nothing_against_the_index() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = Rig::start().await;
    rig.publish("0.42.0");
    let root = rig.install.env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        std::fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        std::fs::set_permissions(root.join(directory), std::fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let (library, capture) = connect_redirect::build(&root);
    let before = pin_overlay::status(
        None,
        &[pin_overlay::Peer::new(
            "peer-remote.netbird.test",
            "100.64.0.21",
            "key-remote",
        )],
    );
    let path = pin_overlay::install(&root.join("bin"), &capture, &before, &before);
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    serve_remote_missing_package(listener, std::sync::Arc::clone(&served));
    let url = rig.url();
    let output = rig
        .install
        .env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .env("POHUNEK_TEST_OFFICIAL_INDEX_URL_V2", &url)
        .args([
            "--host",
            "peer-remote",
            "assistant",
            "--project",
            "demo",
            "--no-snapshot",
            "--no-start-daemon",
            "--yes",
            "--print-prompt",
            "--json",
            "help me",
        ])
        .output()
        .expect("run pohunek assistant");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    let document: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout is one JSON document ({error}): {combined}"));
    // The fixture daemon answers through a real connection, so the remote
    // recovery branch runs: the error keeps its stable code, and the human
    // line plus the JSON remedy name the command to run on that host.
    assert!(!output.status.success(), "{combined}");
    assert_eq!(
        document["err"]["code"], "assistant_package_missing",
        "{combined}"
    );
    assert!(
        combined.contains("pohunek assistant setup"),
        "the remedy names the runnable command: {combined}"
    );
    assert!(
        combined.contains("peer-remote"),
        "the remedy names the host to run it on: {combined}"
    );
    for needle in ["<archive>", "<digest>", "--sha256 <digest>"] {
        assert!(
            !combined.contains(needle),
            "no stale placeholder reaches the output: {combined}"
        );
    }
    let served = served.lock().expect("served requests").clone();
    assert!(
        served
            .iter()
            .any(|served| served == protocol::method::ASSISTANT_MATERIALIZE),
        "materialize reached the fixture daemon: {served:?}"
    );
    assert!(
        !served
            .iter()
            .any(|served| served == protocol::method::PACKAGE_INSTALL),
        "no package mutation reached the fixture daemon: {served:?}"
    );
    assert!(
        rig.server.requests().is_empty(),
        "no install is attempted against the index: {:?}",
        rig.server.requests()
    );
    assert!(rig.listed().await.is_empty(), "nothing installed");
}

#[tokio::test]
async fn s5_old_placeholder_hint_never_reaches_the_output() {
    // The unit tests in `assistant::tests` prove the stable-code match and
    // the remedy replacement against a synthetic N-1 error carrying
    // `<archive> --sha256 <digest>`. This scenario proves the same property
    // end to end: every `assistant_package_missing` failure the CLI prints
    // — including the index and daemon errors below — carries the plan's
    // runnable outcome, never a placeholder.
    let rig = Rig::start().await;
    rig.publish_nothing();
    let (code, document, stderr) = rig.assistant(&["--yes", "help me"]);
    assert_ne!(code, 0, "{document} / {stderr}");
    assert_eq!(
        document["err"]["code"], "assistant_package_missing",
        "{document} / {stderr}"
    );
    let combined = format!("{document}{stderr}");
    assert!(
        combined.contains("pohunek assistant setup"),
        "the runnable command reaches the output: {combined}"
    );
    for needle in ["<archive>", "<digest>", "<sha256>", "<digest-from"] {
        assert!(
            !combined.contains(needle),
            "no placeholder reaches the output: {combined}"
        );
    }
}
