//! Scenarios for `cargo xtask plugin features-check`, driven through the real
//! `xtask` binary in a hermetic environment with a fixture checkout.
//!
//! The expected results are stated outside the code under test: the registry
//! is a hand-written `features.txt` snapshot, the fixture archive is built
//! with the real `plugin build`, and the index document is a hand-written
//! envelope the package crate's key-free parser accepts. No test touches the
//! network: an entry `url` is a `file://` path to the fixture archive, which
//! is why `file://` exists (it is refused for a fetched official index).
//!
//! Each source has a scenario that fails without it:
//! - an index-only failure: `runtime-packages/` requires only a present
//!   feature while a fixture index entry's archive descriptor requires the
//!   missing one (exit 1, naming the feature, the package id, the version
//!   and the index source);
//! - a local-only failure: a `runtime-packages/` descriptor requires the
//!   missing feature while the fixture index is empty (exit 1, naming the
//!   local descriptor path);
//! - the same tree with the feature present passes (exit 0);
//! - an entry whose bytes differ from the entry's digest fails (exit 1);
//! - an entry whose descriptor names another package id fails (exit 1);
//! - a fetch failure warns on stderr and the check passes on the other
//!   sources (exit 0), but a `file://` entry archive over the size cap is
//!   refused content and fails (exit 1);
//! - a context entry is skipped and never requires a feature;
//! - the real tree (the real registry snapshot and `runtime-packages/` with
//!   an empty fixture index) passes.

// Rust guideline compliant 2026-11-16

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use pohunek_test_support::env::TestEnv;

/// Feature the failing fixture registry lacks and the fixture descriptor
/// requires.
const MISSING_FEATURE: &str = "launch-env-v1";

/// A feature the fixture registry holds, so the other source requires
/// nothing missing.
const PRESENT_FEATURE: &str = "placement-v1";

/// Package id, runtime id and version of the fixture archive.
const PACKAGE_ID: &str = "pohunek.runtime.kitfixture";
const RUNTIME_ID: &str = "kitfixture";
const VERSION: &str = "0.1.0";

/// Minimal registry snapshot in the `<id> <since>` shape of the real
/// snapshot, without the required feature.
const REGISTRY_WITHOUT: &str = "config-home-subdir-v1 0.38.0\nplacement-v1 0.40.0\n";

/// The same snapshot with the required feature present.
const REGISTRY_WITH: &str =
    "config-home-subdir-v1 0.38.0\nplacement-v1 0.40.0\nlaunch-env-v1 0.38.0\n";

fn text(path: &Path, stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    format!(
        "run of {}:\nstdout:\n{stdout}\nstderr:\n{stderr}",
        path.display()
    )
}

/// A hermetic fixture checkout: the registry snapshot and the checkout
/// marker `xtask` refuses to run without.
struct Fixture {
    env: TestEnv,
}

impl Fixture {
    fn new(registry: &str) -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        let root = env.cwd();
        let snapshot = root.join("crates/daemon/src/agent/host/fixtures/features.txt");
        fs::create_dir_all(snapshot.parent().expect("parent")).expect("fixture dirs");
        fs::write(snapshot, registry).expect("write the registry snapshot");
        fs::create_dir_all(root.join("runtime-packages")).expect("package dir");
        let marker = root.join("crates/xtask/Cargo.toml");
        fs::create_dir_all(marker.parent().expect("parent")).expect("marker dir");
        fs::write(marker, "").expect("checkout marker");
        Self { env }
    }

    /// A package source outside `runtime-packages/` whose descriptor
    /// requires `requires` (the `xtask plugin` package fixture with
    /// `requires` inserted after `runtime_api`).
    fn variant_source(&self, name: &str, requires: &str) -> PathBuf {
        let dir = self.root().join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("remove the source");
        }
        fs::create_dir_all(&dir).expect("source dir");
        let fixture = pohunek_test_support::manifest_dir().join("src/plugin/fixtures/package");
        for file in ["runtime.toml", "detect.toml"] {
            let mut content = fs::read_to_string(fixture.join(file)).expect("read the fixture");
            if file == "runtime.toml" {
                content = content.replacen(
                    "runtime_api = 1",
                    &format!("runtime_api = 1\nrequires = [{requires}]"),
                    1,
                );
            }
            fs::write(dir.join(file), content).expect("write the source");
        }
        dir
    }

    /// A package directory under the checkout's `runtime-packages/` whose
    /// descriptor requires `requires`.
    fn local_package(&self, name: &str, requires: &str) -> PathBuf {
        let dir = self.root().join("runtime-packages").join(name);
        fs::create_dir_all(&dir).expect("package dir");
        let source = pohunek_test_support::manifest_dir().join("src/plugin/fixtures/package");
        for file in ["runtime.toml", "detect.toml"] {
            let mut content = fs::read_to_string(source.join(file)).expect("read the fixture");
            if file == "runtime.toml" {
                content = content.replacen(
                    "runtime_api = 1",
                    &format!("runtime_api = 1\nrequires = [{requires}]"),
                    1,
                );
            }
            fs::write(dir.join(file), content).expect("write the variant");
        }
        dir
    }

    /// Builds source `package` into the scratch `out` directory; returns the
    /// archive path and the digest `plugin build` reports.
    fn build(&self, package: &Path, out: &str) -> (PathBuf, String) {
        let out = self.root().join(out);
        fs::create_dir(&out).expect("create the output directory");
        let output = self.xtask(&[
            "plugin",
            "build",
            "--dir",
            package.to_str().expect("utf-8 path"),
            "--output-dir",
            out.to_str().expect("utf-8 path"),
        ]);
        assert!(
            output.status.success(),
            "{}",
            text(package, &output.stdout, &output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let fields: Vec<&str> = stdout.trim_end().split(' ').collect();
        assert_eq!(fields.len(), 7, "one line of seven fields: {stdout:?}");
        assert_eq!(fields[1], "build");
        assert_eq!(fields[2], "ok:");
        assert_eq!(fields[3], PACKAGE_ID);
        assert_eq!(fields[4], VERSION);
        let archive = out.join(format!("{PACKAGE_ID}-{VERSION}.tar.zst"));
        assert!(archive.is_file(), "plugin build wrote the archive");
        (archive, fields[5].to_owned())
    }

    /// An index document with one `runtime` entry for `archive`.
    fn runtime_index(&self, name: &str, digest: &str, archive: &Path) -> PathBuf {
        let entry = format!(
            "{{\n        \"package_id\": \"{PACKAGE_ID}\",\n        \"kind\": \"runtime\",\n        \"runtime_id\": \"{RUNTIME_ID}\",\n        \"version\": \"{VERSION}\",\n        \"digest\": \"{digest}\",\n        \"url\": \"file://{}\",\n        \"platforms\": [\"any\"],\n        \"core\": \">=0.0.0\"\n      }}",
            archive.to_str().expect("utf-8 path"),
        );
        self.index(name, &entry)
    }

    /// An index document with one `context` entry for `archive`.
    fn context_index(&self, name: &str, digest: &str, archive: &Path) -> PathBuf {
        let entry = format!(
            "{{\n        \"package_id\": \"pohunek.context.kitfixture\",\n        \"kind\": \"context\",\n        \"runtime_id\": null,\n        \"version\": \"{VERSION}\",\n        \"digest\": \"{digest}\",\n        \"url\": \"file://{}\",\n        \"platforms\": [\"any\"],\n        \"core\": \">=0.0.0\"\n      }}",
            archive.to_str().expect("utf-8 path"),
        );
        self.index(name, &entry)
    }

    /// An index document whose `entries` is exactly `entry`.
    fn index(&self, name: &str, entry: &str) -> PathBuf {
        let signature = "00".repeat(64);
        let document = format!(
            "{{\n  \"schema_version\": 2,\n  \"index\": {{\n    \"schema_version\": 2,\n    \"scope\": \"official\",\n    \"sequence\": 1,\n    \"expires_at\": 4102444800,\n    \"entries\": [\n{entry}\n    ]\n  }},\n  \"key_records\": [],\n  \"signatures\": [],\n  \"revocations\": {{\n    \"revocations\": {{\"schema_version\": 1, \"sequence\": 1, \"revoked_key_ids\": []}},\n    \"signature\": {{\"key_id\": \"f52cf66e089130ee817e18db5b52db21ee6b002c29615f47d80b168931ba89fe\", \"signature\": \"{signature}\"}}\n  }}\n}}\n",
        );
        let path = self.root().join(name);
        fs::write(&path, document).expect("write the index");
        path
    }

    /// An index document with no entries.
    fn empty_index(&self, name: &str) -> PathBuf {
        let signature = "00".repeat(64);
        let document = format!(
            "{{\n  \"schema_version\": 2,\n  \"index\": {{\n    \"schema_version\": 2,\n    \"scope\": \"official\",\n    \"sequence\": 1,\n    \"expires_at\": 4102444800,\n    \"entries\": []\n  }},\n  \"key_records\": [],\n  \"signatures\": [],\n  \"revocations\": {{\n    \"revocations\": {{\"schema_version\": 1, \"sequence\": 1, \"revoked_key_ids\": []}},\n    \"signature\": {{\"key_id\": \"f52cf66e089130ee817e18db5b52db21ee6b002c29615f47d80b168931ba89fe\", \"signature\": \"{signature}\"}}\n  }}\n}}\n",
        );
        let path = self.root().join(name);
        fs::write(&path, document).expect("write the index");
        path
    }

    fn check(&self, index: &Path) -> Output {
        self.xtask(&[
            "plugin",
            "features-check",
            "--index",
            index.to_str().expect("utf-8 path"),
        ])
    }

    fn root(&self) -> &Path {
        self.env.cwd()
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .current_dir(self.root())
            .args(args)
            .output()
            .expect("run xtask")
    }
}

#[test]
fn index_only_failure_fails_naming_the_feature_and_the_package() {
    let fixture = Fixture::new(REGISTRY_WITHOUT);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("requiring-source", &format!("\"{MISSING_FEATURE}\""));
    let (archive, digest) = fixture.build(&source, "out");
    let index = fixture.runtime_index("index.json", &digest, &archive);
    let output = fixture.check(&index);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains(MISSING_FEATURE),
        "names the feature: {stderr:?}"
    );
    assert!(stderr.contains(PACKAGE_ID), "names the package: {stderr:?}");
    assert!(stderr.contains(VERSION), "names the version: {stderr:?}");
    assert!(
        stderr.contains("(index archive)"),
        "names the index source: {stderr:?}"
    );
    assert!(
        output.stdout.is_empty(),
        "a failure prints no report: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn local_only_failure_fails_naming_the_local_descriptor() {
    let fixture = Fixture::new(REGISTRY_WITHOUT);
    fixture.local_package("requiring", &format!("\"{MISSING_FEATURE}\""));
    let index = fixture.empty_index("empty.json");
    let output = fixture.check(&index);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains(MISSING_FEATURE),
        "names the feature: {stderr:?}"
    );
    assert!(
        stderr.contains("runtime-packages/requiring/runtime.toml"),
        "names the local descriptor: {stderr:?}"
    );
}

#[test]
fn present_feature_passes() {
    let fixture = Fixture::new(REGISTRY_WITH);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("requiring-source", &format!("\"{MISSING_FEATURE}\""));
    let (archive, digest) = fixture.build(&source, "out");
    let index = fixture.runtime_index("index.json", &digest, &archive);
    let output = fixture.check(&index);
    assert!(
        output.status.success(),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(
        stdout.starts_with("plugin features-check ok:"),
        "a pass prints the report: {stdout:?}"
    );
}

#[test]
fn digest_mismatch_fails() {
    let fixture = Fixture::new(REGISTRY_WITHOUT);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("tampered-source", &format!("\"{MISSING_FEATURE}\""));
    let (archive, _digest) = fixture.build(&source, "out");
    let tampered = format!("sha256:{}", "00".repeat(32));
    let index = fixture.runtime_index("index.json", &tampered, &archive);
    let output = fixture.check(&index);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains(PACKAGE_ID), "names the package: {stderr:?}");
}

#[test]
fn descriptor_identity_mismatch_fails() {
    let fixture = Fixture::new(REGISTRY_WITH);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("other-source", &format!("\"{PRESENT_FEATURE}\""));
    let (archive, digest) = fixture.build(&source, "out");
    let other = format!(
        "{{\n        \"package_id\": \"pohunek.runtime.other\",\n        \"kind\": \"runtime\",\n        \"runtime_id\": \"other\",\n        \"version\": \"{VERSION}\",\n        \"digest\": \"{digest}\",\n        \"url\": \"file://{}\",\n        \"platforms\": [\"any\"],\n        \"core\": \">=0.0.0\"\n      }}",
        archive.to_str().expect("utf-8 path"),
    );
    let index = fixture.index("index.json", &other);
    let output = fixture.check(&index);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains("names a package id or version the entry does not list"),
        "pins the reason: {stderr:?}"
    );
}

#[test]
fn unreachable_archive_warns_and_passes_on_the_other_sources() {
    let fixture = Fixture::new(REGISTRY_WITH);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("missing-source", &format!("\"{PRESENT_FEATURE}\""));
    let (_archive, digest) = fixture.build(&source, "missing-build");
    let missing = fixture.root().join("missing-build").join("missing.tar.zst");
    let index = fixture.runtime_index("index.json", &digest, &missing);
    let output = fixture.check(&index);
    assert!(
        output.status.success(),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("warn:"), "a failed fetch warns: {stderr:?}");
    assert!(
        stderr.contains(PACKAGE_ID),
        "the warning names the package: {stderr:?}"
    );
}

/// An over-limit `file://` entry archive is refused content and fails the
/// check (exit 1), unlike the missing file of the warn sibling above. The
/// size check fires before the digest is ever read, so the entry's digest
/// never matters here.
#[test]
fn oversized_file_archive_fails() {
    let fixture = Fixture::new(REGISTRY_WITH);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("oversized-source", &format!("\"{PRESENT_FEATURE}\""));
    let (_archive, digest) = fixture.build(&source, "out");
    // One byte over the 8 MiB archive cap the package crate enforces.
    let big = fixture.root().join("out").join("oversized.tar.zst");
    fs::write(&big, vec![0u8; 8 * 1024 * 1024 + 1]).expect("write the oversized file");
    let index = fixture.runtime_index("index.json", &digest, &big);
    let output = fixture.check(&index);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains("larger than"),
        "names the size-cap refusal: {stderr:?}"
    );
    assert!(
        stderr.contains("oversized.tar.zst"),
        "names the refused archive: {stderr:?}"
    );
}

#[test]
fn context_entry_is_skipped() {
    let fixture = Fixture::new(REGISTRY_WITHOUT);
    fixture.local_package("clean", &format!("\"{PRESENT_FEATURE}\""));
    let source = fixture.variant_source("context-source", &format!("\"{MISSING_FEATURE}\""));
    let (archive, digest) = fixture.build(&source, "out");
    let index = fixture.context_index("index.json", &digest, &archive);
    let output = fixture.check(&index);
    assert!(
        output.status.success(),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
}

#[test]
fn real_tree_with_an_empty_index_passes() {
    let workspace = pohunek_test_support::workspace_root();
    let fixture = Fixture::new(REGISTRY_WITH);
    let index = fixture.empty_index("empty.json");
    let output = TestEnv::new()
        .expect("hermetic test environment")
        .command(pohunek_test_support::bin_exe("xtask"))
        .current_dir(&workspace)
        .args([
            "plugin",
            "features-check",
            "--index",
            index.to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run xtask");
    assert!(
        output.status.success(),
        "{}",
        text(&index, &output.stdout, &output.stderr)
    );
}
