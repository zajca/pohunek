//! Behavioural scenarios for `pohunek-plugin-kit`, driven through the real
//! binary. The fixture package passes all six subcommands; every subcommand
//! has one failing case whose revert leaves nothing behind; and a
//! `pohunek.runtime.x` package the throwaway anchor accepts is refused by a
//! host whose index never authorized it.
//!
//! The expected results are stated outside the code under test: digests are
//! recomputed with SHA-256 over the file bytes, exit statuses and evidence
//! shapes are restated as literals, and the isolation is observed from the
//! outside (the temporary directory the proof ran in). No test waits on a
//! production deadline: every wait is bounded by
//! `pohunek_test_support::wait::HANG_GUARD`.
//!
//! - lint, build, verify, detect-check, smoke and prove pass on the fixture;
//! - a second build refuses to replace the archive, a broken descriptor
//!   fails lint, a wrong digest fails verify, a mismatched case fails
//!   detect-check, a missing `--upstream` fails smoke, and `--no-enable`
//!   fails prove with evidence naming the failed step;
//! - a `pohunek.runtime.x` package passes prove through the throwaway anchor
//!   but its install is refused on an isolated host whose index lists
//!   nothing.

// Rust guideline compliant 2026-11-06

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use plugin_kit::isolated::{Host, Root};
use plugin_kit::throwaway;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::HANG_GUARD;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// Package id and version of the fixture package.
const PACKAGE_ID: &str = "pohunek.runtime.kitfixture";
const VERSION: &str = "0.1.0";

/// File name `build` gives the fixture archive.
const ARCHIVE_NAME: &str = "pohunek.runtime.kitfixture-0.1.0.tar.zst";

/// Package id of the variant no index authorizes.
const UNADMITTED_PACKAGE_ID: &str = "pohunek.runtime.x";
const UNADMITTED_RUNTIME_ID: &str = "x";

/// Digest no archive has.
const ABSENT_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Prefix of the directory a proof creates in `TMPDIR`.
const PROOF_ROOT_PREFIX: &str = "pohunek-prove-";

fn s(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("write to a String");
        text
    })
}

fn digest_of(path: &Path) -> String {
    format!(
        "sha256:{}",
        hex(&Sha256::digest(fs::read(path).expect("read")))
    )
}

fn fixture_dir() -> PathBuf {
    pohunek_test_support::manifest_dir().join("tests/fixtures/kit-package")
}

/// Release binaries the isolated daemon copies beside the kit binary; a
/// change to any of them must re-run the kit suite (`BINARY_USES`).
const POHUNEK_BIN: &str = "pohunek";
const POHUNEKD_BIN: &str = "pohunekd";
const SESSIOND_BIN: &str = "pohunek-sessiond";

/// The directory holding the workspace binaries the isolated daemon copies.
/// Each `with_file_name` names a release binary literally so the affected
/// guard sees the run (`BINARY_USES`); a change to any of them re-runs the
/// kit suite.
fn release_dir() -> PathBuf {
    let kit = pohunek_test_support::bin_exe("pohunek-plugin-kit");
    for path in [
        kit.with_file_name(POHUNEK_BIN),
        kit.with_file_name(POHUNEKD_BIN),
        kit.with_file_name(SESSIOND_BIN),
    ] {
        let name = path
            .file_name()
            .expect("a binary file name")
            .to_string_lossy()
            .into_owned();
        assert!(path.is_file(), "{name} is missing; build it with `cargo build -p pohunek-cli -p pohunek-daemon -p pohunek-session-worker`");
    }
    kit.parent()
        .expect("the kit binary has a directory")
        .to_path_buf()
}

/// A hermetic environment with the checkout marker the kit inherits.
struct Kit {
    env: TestEnv,
}

impl Kit {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        let marker = env.cwd().join("crates/xtask/Cargo.toml");
        fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
        fs::write(marker, "").expect("marker");
        Self { env }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.env.root().join(name)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        fs::create_dir(&path).expect("create the directory");
        path
    }

    /// Runs the kit binary with every wait bounded by `HANG_GUARD`: a hung
    /// kit is killed and the failure names its stderr.
    fn kit(&self, args: &[&str]) -> Output {
        use std::io::Read as _;
        use std::process::Stdio;
        use std::time::Instant;
        let mut child = self
            .env
            .command(pohunek_test_support::bin_exe("pohunek-plugin-kit"))
            .current_dir(self.env.cwd())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run pohunek-plugin-kit");
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            match child.try_wait().expect("poll the kit") {
                Some(status) => {
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(mut pipe) = child.stdout.take() {
                        pipe.read_to_end(&mut stdout).expect("read stdout");
                    }
                    if let Some(mut pipe) = child.stderr.take() {
                        pipe.read_to_end(&mut stderr).expect("read stderr");
                    }
                    return Output {
                        status,
                        stdout,
                        stderr,
                    };
                }
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("pohunek-plugin-kit {args:?} hung past {HANG_GUARD:?}");
                }
                None => {
                    // timing-allowed: #1613 the kit child poll sleeps under HANG_GUARD.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }

    /// A copy of the fixture package whose descriptor text has `from`
    /// replaced by `to`.
    fn variant(&self, name: &str, from: &str, to: &str) -> PathBuf {
        let dir = self.dir(name);
        for file in ["runtime.toml", "detect.toml"] {
            let text = fs::read_to_string(fixture_dir().join(file)).expect("read the fixture");
            let text = if file == "runtime.toml" {
                assert!(text.contains(from), "the fixture has no `{from}`");
                text.replace(from, to)
            } else {
                text
            };
            fs::write(dir.join(file), text).expect("write the variant");
        }
        for screen in ["idle_prompt.txt", "blocked_input.txt"] {
            fs::copy(fixture_dir().join("screens").join(screen), dir.join(screen))
                .expect("copy the screen");
        }
        dir
    }

    /// A copy of the fixture package renamed to the unadmitted
    /// `pohunek.runtime.x` identity.
    fn unadmitted_package(&self, name: &str) -> PathBuf {
        let dir = self.dir(name);
        for file in ["runtime.toml", "detect.toml"] {
            let text = fs::read_to_string(fixture_dir().join(file)).expect("read the fixture");
            fs::write(dir.join(file), text).expect("write the variant");
        }
        for screen in ["idle_prompt.txt", "blocked_input.txt"] {
            fs::copy(fixture_dir().join("screens").join(screen), dir.join(screen))
                .expect("copy the screen");
        }
        let descriptor = fixture_dir().join("runtime.toml");
        let text = fs::read_to_string(descriptor).expect("read the descriptor");
        let text = text.replace(PACKAGE_ID, UNADMITTED_PACKAGE_ID).replace(
            &format!("id = \"{}\"", "kitfixture"),
            &format!("id = \"{UNADMITTED_RUNTIME_ID}\""),
        );
        assert!(text.contains(UNADMITTED_PACKAGE_ID));
        fs::write(dir.join("runtime.toml"), text).expect("write the descriptor");
        dir
    }

    /// Builds `package` into a new directory and returns the archive path.
    fn build(&self, package: &Path, out: &str) -> PathBuf {
        let out = self.dir(out);
        let output = self.kit(&["build", "--dir", s(package), "--output-dir", s(&out)]);
        assert!(
            output.status.success(),
            "kit build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert!(
            stdout.starts_with("plugin build ok: "),
            "unexpected build line: {stdout:?}"
        );
        let archive = fs::read_dir(&out)
            .expect("read the output")
            .map(|item| item.expect("entry").path())
            .find(|path| path.extension().is_some_and(|ext| ext == "zst"))
            .expect("the archive exists");
        assert_eq!(
            digest_of(&archive),
            stdout.trim_end().split(' ').nth(5).expect("digest field")
        );
        archive
    }

    /// Proof roots left in `TMPDIR`.
    fn leftover_roots(&self) -> Vec<String> {
        fs::read_dir(self.env.tmp_dir())
            .expect("read TMPDIR")
            .map(|item| {
                item.expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(PROOF_ROOT_PREFIX))
            .collect()
    }
}

fn evidence_of(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("the evidence file exists"))
        .expect("the evidence is JSON")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn kit_lint_build_verify_and_detect_check_pass_on_fixture() {
    let kit = Kit::new();
    let package = fixture_dir();

    let output = kit.kit(&["lint", "--dir", s(&package)]);
    assert!(
        output.status.success(),
        "lint failed: {}",
        stderr_of(&output)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8 stdout"),
        format!("plugin lint ok: {PACKAGE_ID} {VERSION} kitfixture\n")
    );

    let archive = kit.build(&package, "out");
    assert_eq!(
        archive
            .file_name()
            .expect("file name")
            .to_str()
            .expect("utf-8"),
        ARCHIVE_NAME
    );

    let output = kit.kit(&[
        "verify",
        "--dir",
        s(&package),
        "--archive",
        s(&archive),
        "--sha256",
        &digest_of(&archive),
    ]);
    assert!(
        output.status.success(),
        "verify failed: {}",
        stderr_of(&output)
    );
    assert!(String::from_utf8(output.stdout)
        .expect("utf-8 stdout")
        .starts_with(&format!("plugin verify ok: {PACKAGE_ID} {VERSION} sha256:")));

    // Without `--dir`, verify only re-reads the archive and reports its
    // digest; without `--sha256`, the digest is compared against nothing.
    let digest = digest_of(&archive);
    let output = kit.kit(&["verify", "--archive", s(&archive)]);
    assert!(
        output.status.success(),
        "verify without --dir failed: {}",
        stderr_of(&output)
    );
    assert!(String::from_utf8(output.stdout)
        .expect("utf-8 stdout")
        .contains(digest.strip_prefix("sha256:").expect("a sha256 digest")));

    let output = kit.kit(&[
        "detect-check",
        "--dir",
        s(&package),
        "--corpus",
        s(&package),
    ]);
    assert!(
        output.status.success(),
        "detect-check failed: {}",
        stderr_of(&output)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8 stdout"),
        format!("plugin detect-check ok: {PACKAGE_ID} 2 cases\n")
    );

    assert!(
        kit.leftover_roots().is_empty(),
        "lint/build/verify/detect-check leave no roots"
    );
}

#[test]
fn kit_smoke_passes_on_fixture() {
    let kit = Kit::new();
    let release = release_dir();
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
    ]);
    assert!(
        output.status.success(),
        "smoke failed: {}",
        stderr_of(&output)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(
        stdout.starts_with(&format!("plugin smoke ok: {PACKAGE_ID} {VERSION} idle ")),
        "unexpected smoke line: {stdout:?}"
    );
    assert!(stdout.trim_end().ends_with("screen lines"));
    assert!(kit.leftover_roots().is_empty(), "smoke removes its root");
}

#[test]
fn kit_prove_passes_on_fixture() {
    let kit = Kit::new();
    let release = release_dir();
    let evidence = kit.path("prove.json");
    let output = kit.kit(&[
        "prove",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--evidence",
        s(&evidence),
    ]);
    assert!(
        output.status.success(),
        "prove failed: {}",
        stderr_of(&output)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(
        stdout.starts_with(&format!("plugin prove ok: {PACKAGE_ID} {VERSION} sha256:")),
        "unexpected prove line: {stdout:?}"
    );
    let evidence = evidence_of(&evidence);
    assert_eq!(evidence["result"], "pass");
    assert_eq!(evidence["package_id"], PACKAGE_ID);
    assert!(evidence.get("failed_step").is_some_and(Value::is_null));
    let kit_sha256 = evidence["kit_sha256"]
        .as_str()
        .expect("the evidence names the kit digest");
    let digest = kit_sha256.strip_prefix("sha256:").expect("a sha256 digest");
    assert_eq!(digest.len(), 64, "the kit digest is 64 hex chars");
    assert!(
        digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "the kit digest is hex: {kit_sha256:?}"
    );
    assert_eq!(
        kit_sha256,
        &digest_of(&pohunek_test_support::bin_exe("pohunek-plugin-kit")),
        "the evidence names the kit binary the test ran"
    );
    assert!(kit.leftover_roots().is_empty(), "prove removes its root");
}

#[test]
fn kit_build_refuses_to_replace_the_archive() {
    let kit = Kit::new();
    let archive = kit.build(&fixture_dir(), "out");
    let before = digest_of(&archive);
    let output = kit.kit(&[
        "build",
        "--dir",
        s(&fixture_dir()),
        "--output-dir",
        s(archive.parent().expect("parent")),
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("File exists"),
        "the refusal names the existing file: {}",
        stderr_of(&output)
    );
    assert_eq!(digest_of(&archive), before, "the archive is unchanged");
}

#[test]
fn kit_lint_refuses_a_broken_descriptor() {
    let kit = Kit::new();
    let package = kit.variant("broken", PACKAGE_ID, PACKAGE_ID);
    fs::write(package.join("runtime.toml"), "NOT TOML [[[\n").expect("break the descriptor");
    let output = kit.kit(&["lint", "--dir", s(&package)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("package lint failed"),
        "the refusal names lint: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_verify_refuses_a_wrong_digest() {
    let kit = Kit::new();
    let archive = kit.build(&fixture_dir(), "out");
    let output = kit.kit(&[
        "verify",
        "--dir",
        s(&fixture_dir()),
        "--archive",
        s(&archive),
        "--sha256",
        ABSENT_DIGEST,
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("differs from `--sha256`"),
        "the refusal names the digest: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_detect_check_refuses_a_mismatched_case() {
    let kit = Kit::new();
    let corpus = kit.dir("corpus");
    let screens = corpus.join("screens");
    fs::create_dir(&screens).expect("screens");
    fs::copy(
        fixture_dir().join("screens").join("idle_prompt.txt"),
        screens.join("idle_prompt.txt"),
    )
    .expect("copy the screen");
    fs::write(
        corpus.join("cases.toml"),
        "[[case]]\nscreen = \"idle_prompt.txt\"\nwidth = 80\nexpect = \"working\"\n",
    )
    .expect("write the corpus");
    let output = kit.kit(&[
        "detect-check",
        "--dir",
        s(&fixture_dir()),
        "--corpus",
        s(&corpus),
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("expected working, observed idle"),
        "the refusal names the mismatch: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_smoke_refuses_a_missing_upstream() {
    let kit = Kit::new();
    let release = release_dir();
    let missing = kit.path("does-not-exist");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--upstream",
        s(&missing),
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("cannot resolve `--upstream`"),
        "the refusal names --upstream: {}",
        stderr_of(&output)
    );
    assert!(
        kit.leftover_roots().is_empty(),
        "a refused smoke leaves no root"
    );
}

#[test]
fn kit_prove_no_enable_fails_with_evidence() {
    let kit = Kit::new();
    let release = release_dir();
    let evidence = kit.path("prove.json");
    let output = kit.kit(&[
        "prove",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--evidence",
        s(&evidence),
        "--no-enable",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "rc is 1: {}",
        stderr_of(&output)
    );
    let evidence = evidence_of(&evidence);
    assert_eq!(evidence["result"], "fail");
    assert_eq!(evidence["package_id"], PACKAGE_ID);
    assert!(
        evidence["failed_step"].is_string(),
        "the evidence names the failed step: {evidence}"
    );
    assert!(
        kit.leftover_roots().is_empty(),
        "a failed proof removes its root"
    );
}

#[test]
fn kit_smoke_accepts_the_unadmitted_runtime_id() {
    let kit = Kit::new();
    let package = kit.unadmitted_package("unadmitted");
    let release = release_dir();
    let output = kit.kit(&["smoke", "--dir", s(&package), "--release-dir", s(&release)]);
    assert!(
        output.status.success(),
        "the throwaway anchor accepts {UNADMITTED_PACKAGE_ID}: {}",
        stderr_of(&output)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(
        stdout.starts_with(&format!(
            "plugin smoke ok: {UNADMITTED_PACKAGE_ID} {VERSION} idle "
        )),
        "unexpected smoke line: {stdout:?}"
    );
    assert!(kit.leftover_roots().is_empty(), "smoke removes its root");
}

#[test]
fn kit_release_host_refuses_the_throwaway_index() {
    let kit = Kit::new();
    let package = kit.unadmitted_package("unadmitted");
    let archive = kit.build(&package, "out");
    let digest = digest_of(&archive);

    // The throwaway index lists the digest, exactly as a proof would sign
    // it: the refusal must come from the host trusting the release anchor,
    // not from the index omitting the archive.
    let trust = kit.dir("trust");
    let entry = plugin_kit::entry::EntryFile {
        package_id: UNADMITTED_PACKAGE_ID.parse().expect("package id"),
        kind: None,
        runtime_id: Some(UNADMITTED_RUNTIME_ID.parse().expect("runtime id")),
        version: VERSION.parse().expect("version"),
        digest: digest.parse().expect("digest"),
        url: "https://localhost/kit-refusal".to_owned(),
        platforms: vec![package::host_platform().expect("platform name")],
        core: ">=0.0.0".to_owned(),
        source: plugin_kit::entry::Source::prove_placeholder(&VERSION.parse().expect("version")),
    }
    .signed_entry(package::INDEX_SCHEMA_VERSION)
    .expect("the entry signs");
    throwaway::write(&throwaway::ThrowawayArgs {
        entries: vec![entry],
        output: trust.clone(),
        schema_version: 1,
    })
    .expect("the throwaway index lists the digest");
    let index = trust.join(package::OFFICIAL_INDEX_FILE_NAME);
    assert!(index.is_file(), "throwaway writes the index");

    // The host trusts the committed release anchor and channel, never the
    // throwaway root: the throwaway-signed index is foreign to it.
    let workspace = pohunek_test_support::workspace_root();
    let root = Root::create_in(kit.env.tmp_dir()).expect("the isolation root");
    let mut host = Host::prepare(
        &root,
        &release_dir(),
        &workspace.join("packaging/runtime-catalog-anchor.json"),
        &workspace.join("packaging/plugin-index-channel.json"),
    )
    .expect("the isolated host");
    host.launch_local_only(&root)
        .expect("the local-only daemon");
    let output = host
        .cli(&[
            "plugin",
            "install",
            s(&archive),
            "--index",
            s(&index),
            "--yes",
            "--json",
        ])
        .expect("the install runs");
    assert_ne!(output.code, Some(0), "the install is refused");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("package_untrusted"),
        "the release-anchored host refuses the throwaway index: {stdout:?}"
    );
    host.stop().expect("the daemon stops");
    root.remove().expect("the root is removed");
    assert!(
        kit.leftover_roots().is_empty(),
        "the refused install leaves no root"
    );
}
