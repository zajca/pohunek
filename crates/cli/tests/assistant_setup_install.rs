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
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
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

#[tokio::test]
async fn s4_remote_host_installs_nothing_against_the_index() {
    let rig = Rig::start().await;
    rig.publish("0.42.0");
    let root = rig.install.env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        std::fs::create_dir_all(root.join(directory)).expect("create the test directory");
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
    let url = rig.url();
    let output = rig
        .install
        .env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", "9")
        .env("POHUNEK_TEST_CONNECT_REFUSE", "1")
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
    assert!(
        !output.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
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
    let rig = Rig::start().await;
    rig.publish("0.42.0");
    let (code, document, stderr) = rig.assistant(&["--yes", "help me"]);
    assert_eq!(code, 0, "{document} / {stderr}");
    let combined = format!("{document}{stderr}");
    for needle in ["<archive>", "<digest>", "<sha256>", "<digest-from"] {
        assert!(
            !combined.contains(needle),
            "no placeholder reaches the output: {combined}"
        );
    }
}
