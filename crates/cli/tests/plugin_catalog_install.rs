//! End-to-end contracts of the production catalog trust anchor.
//!
//! Every test starts a real `pohunekd` copied into a clean directory beside
//! the anchor file the scenario gives it, in a hermetic XDG/HOME environment,
//! and drives the real `pohunek plugin install --catalog` against it. The
//! daemon reads its anchor the way a release install does (from the directory
//! of its own executable), so these tests cover anchor resolution, the
//! file-safety policy, catalog verification and the fail-closed paths end to
//! end. Signing keys are generated inside each test process from random bytes
//! and never leave it.

// Rust guideline compliant 2026-10-06

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use package::{
    build_archive, read_archive, ArchiveEntry, CatalogEntry, Limits, PackageDigest,
    ANCHOR_FILE_NAME, MAX_ANCHOR_BYTES,
};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::fs::{write_executable, write_file};
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;
use tokio::net::UnixStream;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;

use catalog_fixture::{
    anchor_for, anchor_with, catalog_of, host_platform, key_id, signed, test_key, ANY_CORE,
    WINDOW_END, WINDOW_START,
};

/// Package id of the fixture archive; it serves a non-reserved runtime id.
const PACKAGE_ID: &str = "acme.runtime.pi";

/// Runtime id the fixture package serves.
const RUNTIME: &str = "pi";

/// Program the fixture runtime launches; it only has to exist.
const PROGRAM: &str = "/bin/sh";

/// Name of the daemon executable copied into the scenario's directory.
const DAEMON_NAME: &str = "pohunekd";

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

fn runtime_document(version: &str) -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE_ID}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{RUNTIME}"
name = "Pi"
program = "{PROGRAM}"
args = []
detect_manifest = "detect.toml"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
check = "none"
"#
    )
}

/// A package archive on disk with the digest the daemon will see.
struct Built {
    path: PathBuf,
    digest: PackageDigest,
}

fn entry_for(built: &Built, version: &str, platform: &str, core: &str) -> CatalogEntry {
    CatalogEntry {
        package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
        runtime_id: RuntimeId::parse(RUNTIME).expect("runtime id"),
        version: PackageVersion::parse(version).expect("version"),
        digest: built.digest.clone(),
        platforms: vec![platform.to_owned()],
        core: core.to_owned(),
    }
}

/// What a scenario puts beside the daemon executable.
enum Anchor {
    /// No anchor file.
    None,
    /// A file with `bytes` and the permission bits `mode`.
    File { bytes: Vec<u8>, mode: u32 },
    /// A symbolic link to a valid anchor stored elsewhere.
    Link { bytes: Vec<u8> },
}

/// A clean installation: a real daemon in a hermetic environment, with the
/// `pohunek` CLI pointed at it.
struct Install {
    env: TestEnv,
    _daemon: tokio::process::Child,
}

impl Install {
    async fn start(anchor: &Anchor) -> Self {
        let env = TestEnv::new().expect("create the hermetic environment");
        let release = env.root().join("release");
        fs::create_dir(&release).expect("create the release directory");
        fs::set_permissions(&release, fs::Permissions::from_mode(0o755))
            .expect("release directory mode");
        let daemon = release.join(DAEMON_NAME);
        let source = bin_exe("pohunek").with_file_name(DAEMON_NAME);
        assert!(
            source.is_file(),
            "{} is missing; build it with `cargo build -p pohunek-daemon --bins`",
            source.display()
        );
        write_executable(&daemon, fs::read(&source).expect("read the daemon binary"))
            .expect("install the daemon binary");
        let anchor_path = release.join(ANCHOR_FILE_NAME);
        match anchor {
            Anchor::None => {}
            Anchor::File { bytes, mode } => {
                write_file(&anchor_path, bytes).expect("write the anchor");
                fs::set_permissions(&anchor_path, fs::Permissions::from_mode(*mode))
                    .expect("anchor mode");
            }
            Anchor::Link { bytes } => {
                let real = env.root().join("real-anchor.json");
                write_file(&real, bytes).expect("write the anchor target");
                fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).expect("anchor mode");
                symlink(&real, &anchor_path).expect("link the anchor");
            }
        }
        let mut command = env.tokio_command(&daemon);
        command
            .env("SHELL", "/bin/sh")
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn the daemon");
        let socket = env.runtime_dir().join("pohunek/daemon.sock");
        let connected = wait_until("the daemon control socket accepts connections", || async {
            UnixStream::connect(&socket).await.ok()
        });
        tokio::select! {
            _stream = connected => {}
            status = child.wait() => panic!("daemon exited before readiness: {status:?}"),
        }
        Self {
            env,
            _daemon: child,
        }
    }

    fn archive(&self, version: &str) -> Built {
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: runtime_document(version).into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let path = self.env.root().join(format!("pi-{version}.tar.zst"));
        fs::write(&path, bytes).expect("write the archive");
        Built { path, digest }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.env.root().join(name);
        fs::write(&path, bytes).expect("write the file");
        path
    }

    /// Runs `pohunek <arguments> --json` and returns the exit code and the
    /// single JSON document on stdout.
    async fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self
            .env
            .tokio_command(bin_exe("pohunek"))
            .args(&all)
            .output()
            .await
            .expect("run the pohunek binary");
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {} / {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().expect("exit code"), document)
    }

    /// `plugin install <archive> --catalog <catalog> --yes`.
    async fn install_catalog(&self, built: &Built, catalog: &Path) -> (i32, Value) {
        self.json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--catalog",
            path_str(catalog),
            "--yes",
        ])
        .await
    }

    /// The installed packages as `plugin list` reports them.
    async fn packages(&self) -> Vec<Value> {
        let (code, document) = self.json(&["plugin", "list"]).await;
        assert_eq!(code, 0, "{document}");
        document["ok"]["packages"]
            .as_array()
            .expect("packages")
            .clone()
    }

    /// The `catalog_trust_anchor` check of `pohunek doctor`.
    async fn trust_check(&self) -> Value {
        let (_code, document) = self.json(&["doctor"]).await;
        document["ok"]["checks"]
            .as_array()
            .unwrap_or_else(|| panic!("doctor lists checks: {document}"))
            .iter()
            .find(|check| check["name"] == "catalog_trust_anchor")
            .unwrap_or_else(|| panic!("doctor reports the trust anchor: {document}"))
            .clone()
    }

    /// Asserts a refused install: the typed error code, nothing recorded.
    async fn assert_refused(&self, outcome: (i32, Value), code: &str) {
        let (exit, document) = outcome;
        assert_eq!(exit, 1, "{document}");
        assert_eq!(document["err"]["code"], code, "{document}");
        assert!(
            self.packages().await.is_empty(),
            "a refused catalog install records nothing"
        );
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

#[tokio::test]
async fn a_release_catalog_installs_the_package_as_official_in_a_clean_environment() {
    let key = test_key();
    let install = Install::start(&Anchor::File {
        bytes: anchor_for(&key),
        mode: 0o644,
    })
    .await;
    let built = install.archive("1.0.0");
    let catalog = install.write(
        "runtime-catalog.json",
        &signed(
            &key,
            catalog_of(
                5,
                WINDOW_END,
                vec![entry_for(&built, "1.0.0", &host_platform(), ANY_CORE)],
            ),
        ),
    );

    let (code, document) = install.install_catalog(&built, &catalog).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");
    assert_eq!(
        document["ok"]["package"]["origin"], "official",
        "{document}"
    );

    let packages = install.packages().await;
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["origin"], "official");
    assert_eq!(packages[0]["enabled"], true);
    assert_eq!(packages[0]["selected"], true);
    assert_eq!(packages[0]["digest"], built.digest.as_str());

    let (code, hosts) = install.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{hosts}");
    assert!(
        hosts["ok"]["supported_agents"]
            .as_array()
            .expect("supported_agents")
            .iter()
            .any(|agent| agent == RUNTIME),
        "the official package serves its runtime: {hosts}"
    );

    let check = install.trust_check().await;
    assert_eq!(check["status"], "ok", "{check}");

    // The persisted sequence rejects an older catalog for the same archive.
    let older = install.write(
        "older-catalog.json",
        &signed(
            &key,
            catalog_of(
                4,
                WINDOW_END,
                vec![entry_for(&built, "1.0.0", &host_platform(), ANY_CORE)],
            ),
        ),
    );
    let (code, document) = install.install_catalog(&built, &older).await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(document["err"]["code"], "package_untrusted", "{document}");
}

#[tokio::test]
async fn without_an_anchor_file_catalog_installs_stay_unavailable_and_explicit_digests_work() {
    let key = test_key();
    let install = Install::start(&Anchor::None).await;
    let built = install.archive("1.0.0");
    let catalog = install.write(
        "runtime-catalog.json",
        &signed(
            &key,
            catalog_of(
                1,
                WINDOW_END,
                vec![entry_for(&built, "1.0.0", &host_platform(), ANY_CORE)],
            ),
        ),
    );

    let outcome = install.install_catalog(&built, &catalog).await;
    install
        .assert_refused(outcome, "official_trust_unavailable")
        .await;
    let check = install.trust_check().await;
    assert_eq!(check["status"], "warn", "{check}");

    let (code, document) = install
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["package"]["origin"], "explicit_digest");
}

#[tokio::test]
async fn an_anchor_that_cannot_be_trusted_fails_closed_and_doctor_reports_it() {
    let key = test_key();
    let valid = anchor_for(&key);
    let other_id = key_id(&test_key());
    let mismatched = String::from_utf8(valid.clone())
        .expect("utf-8")
        .replace(key_id(&key).as_str(), other_id.as_str())
        .into_bytes();
    let scenarios: Vec<(&str, Anchor)> = vec![
        (
            "malformed json",
            Anchor::File {
                bytes: b"{ not json".to_vec(),
                mode: 0o644,
            },
        ),
        (
            "empty file",
            Anchor::File {
                bytes: Vec::new(),
                mode: 0o644,
            },
        ),
        (
            "key id that does not match its key",
            Anchor::File {
                bytes: mismatched,
                mode: 0o644,
            },
        ),
        (
            "oversized file",
            Anchor::File {
                bytes: vec![b' '; MAX_ANCHOR_BYTES + 1],
                mode: 0o644,
            },
        ),
        (
            "group-writable file",
            Anchor::File {
                bytes: valid.clone(),
                mode: 0o664,
            },
        ),
        (
            "world-writable file",
            Anchor::File {
                bytes: valid.clone(),
                mode: 0o666,
            },
        ),
        (
            "symbolic link to a valid anchor",
            Anchor::Link {
                bytes: valid.clone(),
            },
        ),
    ];
    for (label, anchor) in &scenarios {
        let install = Install::start(anchor).await;
        let built = install.archive("1.0.0");
        let catalog = install.write(
            "runtime-catalog.json",
            &signed(
                &key,
                catalog_of(
                    1,
                    WINDOW_END,
                    vec![entry_for(&built, "1.0.0", &host_platform(), ANY_CORE)],
                ),
            ),
        );
        let outcome = install.install_catalog(&built, &catalog).await;
        install
            .assert_refused(outcome, "official_trust_anchor_invalid")
            .await;
        let check = install.trust_check().await;
        assert_eq!(check["status"], "fail", "{label}: {check}");
        let detail = check["detail"].as_str().expect("detail");
        assert!(
            detail.contains("trust anchor"),
            "{label}: the detail names the anchor: {detail}"
        );
        // The daemon keeps serving local trust while the anchor is refused.
        let (code, document) = install
            .json(&[
                "plugin",
                "install",
                path_str(&built.path),
                "--sha256",
                built.digest.as_str(),
                "--yes",
            ])
            .await;
        assert_eq!(code, 0, "{label}: {document}");
    }
}

#[tokio::test]
async fn a_catalog_that_does_not_verify_installs_nothing() {
    let key = test_key();
    let install = Install::start(&Anchor::File {
        bytes: anchor_for(&key),
        mode: 0o644,
    })
    .await;
    let built = install.archive("1.0.0");
    let other_built = install.archive("2.0.0");
    let platform = host_platform();
    let entries = || vec![entry_for(&built, "1.0.0", &platform, ANY_CORE)];

    let good = signed(&key, catalog_of(1, WINDOW_END, entries()));

    // One altered byte of the signed content.
    let tampered = String::from_utf8(good.clone())
        .expect("utf-8")
        .replace("\"sequence\": 1", "\"sequence\": 2");
    assert_ne!(
        tampered.as_bytes(),
        good.as_slice(),
        "the tamper changed the document"
    );
    let path = install.write("tampered.json", tampered.as_bytes());
    let outcome = install.install_catalog(&built, &path).await;
    install.assert_refused(outcome, "package_untrusted").await;

    // Signed by a key the anchor does not hold.
    let path = install.write(
        "other-key.json",
        &signed(&test_key(), catalog_of(1, WINDOW_END, entries())),
    );
    let outcome = install.install_catalog(&built, &path).await;
    install.assert_refused(outcome, "package_untrusted").await;

    // A catalog whose `expires_at` has passed.
    let path = install.write("expired.json", &signed(&key, catalog_of(1, 1, entries())));
    let outcome = install.install_catalog(&built, &path).await;
    install.assert_refused(outcome, "package_untrusted").await;

    // A catalog that revokes the digest of its own entry is invalid as a whole.
    let mut revoking = catalog_of(1, WINDOW_END, entries());
    revoking.revoked_digests = vec![built.digest.clone()];
    let path = install.write("revoked-digest.json", &signed(&key, revoking));
    let outcome = install.install_catalog(&built, &path).await;
    install.assert_refused(outcome, "package_untrusted").await;

    // A catalog that revokes its own signer.
    let mut self_revoking = catalog_of(1, WINDOW_END, entries());
    self_revoking.revoked_key_ids = vec![key_id(&key)];
    let path = install.write("self-revoking.json", &signed(&key, self_revoking));
    let outcome = install.install_catalog(&built, &path).await;
    install.assert_refused(outcome, "package_untrusted").await;

    // A valid catalog that does not list this archive.
    let path = install.write(
        "other-digest.json",
        &signed(
            &key,
            catalog_of(
                1,
                WINDOW_END,
                vec![entry_for(&other_built, "2.0.0", &platform, ANY_CORE)],
            ),
        ),
    );
    let outcome = install.install_catalog(&built, &path).await;
    install.assert_refused(outcome, "package_untrusted").await;

    // A valid catalog entry for another platform or another core range.
    let path = install.write(
        "other-platform.json",
        &signed(
            &key,
            catalog_of(
                1,
                WINDOW_END,
                vec![entry_for(
                    &built,
                    "1.0.0",
                    "riscv64-unknown-linux-gnu",
                    ANY_CORE,
                )],
            ),
        ),
    );
    let outcome = install.install_catalog(&built, &path).await;
    install
        .assert_refused(outcome, "package_incompatible")
        .await;
    let path = install.write(
        "other-core.json",
        &signed(
            &key,
            catalog_of(
                1,
                WINDOW_END,
                vec![entry_for(&built, "1.0.0", &platform, ">=999.0.0")],
            ),
        ),
    );
    let outcome = install.install_catalog(&built, &path).await;
    install
        .assert_refused(outcome, "package_incompatible")
        .await;

    // The unmodified catalog still installs: none of the refusals above left
    // the host in a state that blocks a good one.
    let path = install.write("good.json", &good);
    let (code, document) = install.install_catalog(&built, &path).await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(
        document["ok"]["package"]["origin"], "official",
        "{document}"
    );
}

#[tokio::test]
async fn an_anchor_that_revokes_the_signer_or_whose_root_window_has_passed_refuses_the_catalog() {
    let key = test_key();
    for (label, anchor) in [
        (
            "root revoked by the anchor",
            anchor_with(&key, WINDOW_START, WINDOW_END, vec![key_id(&key)]),
        ),
        (
            "root window in the past",
            anchor_with(&key, WINDOW_START, WINDOW_START + 1, Vec::new()),
        ),
        (
            "root window in the future",
            anchor_with(&key, WINDOW_END - 1, WINDOW_END, Vec::new()),
        ),
    ] {
        let install = Install::start(&Anchor::File {
            bytes: anchor,
            mode: 0o644,
        })
        .await;
        let built = install.archive("1.0.0");
        let catalog = install.write(
            "runtime-catalog.json",
            &signed(
                &key,
                catalog_of(
                    1,
                    WINDOW_END,
                    vec![entry_for(&built, "1.0.0", &host_platform(), ANY_CORE)],
                ),
            ),
        );
        let outcome = install.install_catalog(&built, &catalog).await;
        assert_eq!(outcome.0, 1, "{label}: {}", outcome.1);
        assert_eq!(outcome.1["err"]["code"], "package_untrusted", "{label}");
        assert!(install.packages().await.is_empty(), "{label}");
        // A usable anchor that rejects this catalog is still a loaded anchor.
        let check = install.trust_check().await;
        assert_eq!(check["status"], "ok", "{label}: {check}");
    }
}
