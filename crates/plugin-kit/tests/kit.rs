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

/// The directory holding the workspace binaries the isolated daemon copies.
fn release_dir() -> PathBuf {
    let dir = pohunek_test_support::bin_exe("pohunek-plugin-kit")
        .parent()
        .expect("the kit binary has a directory")
        .to_path_buf();
    for name in ["pohunek", "pohunekd", "pohunek-sessiond"] {
        assert!(
            dir.join(name).is_file(),
            "{} is missing; build it with `cargo build -p pohunek-cli -p pohunek-daemon -p pohunek-session-worker`",
            dir.join(name).display()
        );
    }
    dir
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

    fn kit(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek-plugin-kit"))
            .current_dir(self.env.cwd())
            .args(args)
            .output()
            .expect("run pohunek-plugin-kit")
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
fn kit_throwaway_accepts_the_unadmitted_runtime_id() {
    let kit = Kit::new();
    let package = kit.unadmitted_package("unadmitted");
    let release = release_dir();
    let evidence = kit.path("prove.json");
    let output = kit.kit(&[
        "prove",
        "--dir",
        s(&package),
        "--release-dir",
        s(&release),
        "--evidence",
        s(&evidence),
    ]);
    assert!(
        output.status.success(),
        "the throwaway anchor accepts {UNADMITTED_PACKAGE_ID}: {}",
        stderr_of(&output)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(
        stdout.starts_with(&format!(
            "plugin prove ok: {UNADMITTED_PACKAGE_ID} {VERSION} sha256:"
        )),
        "unexpected prove line: {stdout:?}"
    );
    assert_eq!(evidence_of(&evidence)["result"], "pass");
    assert!(kit.leftover_roots().is_empty(), "prove removes its root");
}

#[test]
fn kit_isolated_host_refuses_the_unadmitted_runtime_id() {
    let kit = Kit::new();
    let package = kit.unadmitted_package("unadmitted");
    let archive = kit.build(&package, "out");

    let root = Root::create().expect("the isolation root");
    let trust = kit.dir("trust");
    let written = throwaway::write(&throwaway::ThrowawayArgs {
        entries: Vec::new(),
        output: trust.clone(),
        schema_version: 1,
    })
    .expect("the empty throwaway index");
    assert_eq!(written.entries, 0);
    let index = trust.join(package::OFFICIAL_INDEX_FILE_NAME);
    assert!(
        index.is_file(),
        "throwaway writes the index beside the anchor"
    );

    let mut host = Host::prepare(
        &root,
        &release_dir(),
        &trust.join(throwaway::ANCHOR_FILE),
        &trust.join(package::INDEX_CHANNEL_FILE_NAME),
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
        "the trust layer refuses the unlisted digest: {stdout:?}"
    );
    host.stop().expect("the daemon stops");
    root.remove().expect("the root is removed");
    assert!(
        kit.leftover_roots().is_empty(),
        "the refused install leaves no root"
    );
}
