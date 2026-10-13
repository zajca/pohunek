//! Scenarios that read every checked-in compatibility lock through the
//! official-package lock reader of `cargo xtask compat`, driven through the
//! real binary.
//!
//! `compat matrix-check` reads the lock of every directory under
//! `runtime-packages/`, and `compat attest`/`compat verify` recompute an
//! attestation from a package archive, its lock and the matrix. Each scenario
//! builds a fixture repository holding the checked-in locks, copied at run
//! time from this workspace, beside one package directory per lock. Expected
//! values come from the lock rules and the fixture, never from the code under
//! test: the matrix holds one row per runtime and declared target, and an
//! attestation's digests are the SHA-256 of the exact archive and lock bytes,
//! hashed here.

// Rust guideline compliant 2026-10-12

use std::path::{Path, PathBuf};
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// Runtimes that must have a checked-in lock under `compat/`; the scenario
/// also requires that no other directory there holds one.
const LOCKED_RUNTIMES: [&str; 4] = ["claude", "codex", "hermes", "pi"];

/// Targets every official package is attested on.
const TARGETS: [&str; 2] = ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"];

/// Executables of one release, whose bytes an attestation's binary set covers.
const BINARIES: [&str; 3] = ["pohunek", "pohunekd", "pohunek-sessiond"];

/// Suite version of the fixture matrix.
const SUITE_VERSION: u32 = 1;

/// A full commit id; the attestation only requires 40 lowercase hex digits.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Hermes release the checked-in lock pins (`compat/hermes/README.md`).
const HERMES_RELEASE: &str = "0.20.0";

/// Directory of the official Hermes package in this workspace.
const HERMES_PACKAGE_DIR: &str = "runtime-packages/hermes";

fn lock_relative(runtime: &str) -> String {
    format!("compat/{runtime}/compatibility-lock.json")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// Fixture repository with the checked-in locks of `runtimes`, one package
/// directory per runtime and a matrix of every runtime and declared target.
struct Repo {
    env: TestEnv,
}

impl Repo {
    fn new(runtimes: &[&str]) -> Self {
        let repo = Self {
            env: TestEnv::new().expect("hermetic test environment"),
        };
        let workspace = pohunek_test_support::workspace_root();
        let mut rows = Vec::new();
        for runtime in runtimes {
            let lock = std::fs::read(workspace.join(lock_relative(runtime)))
                .expect("read the checked-in lock");
            repo.write(&lock_relative(runtime), &lock);
            std::fs::create_dir_all(repo.dir().join("runtime-packages").join(runtime))
                .expect("create package directory");
            for target in TARGETS {
                rows.push(json!({ "runtime": runtime, "target": target }));
            }
        }
        let matrix = json!({ "schema": 1, "suite_version": SUITE_VERSION, "rows": rows });
        repo.write(
            "compat/matrix.json",
            &serde_json::to_vec_pretty(&matrix).expect("matrix"),
        );
        repo
    }

    fn dir(&self) -> &Path {
        self.env.cwd()
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir().join(relative)
    }

    fn write(&self, relative: &str, bytes: &[u8]) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, bytes).expect("write file");
    }

    /// Runs the real `xtask` in the fixture repository.
    fn xtask(&self, args: &[&dyn AsRef<std::ffi::OsStr>]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .args(args)
            .output()
            .expect("run xtask")
    }

    /// Runs `xtask` and requires success; returns stdout.
    fn xtask_ok(&self, args: &[&dyn AsRef<std::ffi::OsStr>]) -> String {
        let output = self.xtask(args);
        assert!(
            output.status.success(),
            "xtask {:?} failed: {}",
            args.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }
}

#[test]
fn every_checked_in_lock_reads_as_an_official_package_lock() {
    let compat = pohunek_test_support::workspace_root().join("compat");
    let mut checked_in = std::fs::read_dir(&compat)
        .expect("read compat/")
        .map(|entry| entry.expect("compat/ entry").path())
        .filter(|dir| dir.join("compatibility-lock.json").is_file())
        .map(|dir| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8 runtime directory")
                .to_owned()
        })
        .collect::<Vec<_>>();
    checked_in.sort();
    assert_eq!(
        checked_in, LOCKED_RUNTIMES,
        "every checked-in lock is covered"
    );
    let repo = Repo::new(&LOCKED_RUNTIMES);
    let stdout = repo.xtask_ok(&[&"compat", &"matrix-check", &"--root", &repo.dir()]);
    assert_eq!(
        stdout,
        format!(
            "compat matrix-check ok: {} rows\n",
            LOCKED_RUNTIMES.len() * TARGETS.len()
        )
    );
}

#[test]
fn the_hermes_package_and_lock_attest_and_verify_on_every_declared_target() {
    let repo = Repo::new(&["hermes"]);
    let archive = repo.path("hermes.tar.zst");
    repo.xtask_ok(&[
        &"package",
        &"build",
        &pohunek_test_support::workspace_root().join(HERMES_PACKAGE_DIR),
        &"--output",
        &archive,
    ]);
    let bin_dir = repo.path("bin");
    let mut executables = serde_json::Map::new();
    for name in BINARIES {
        let bytes = format!("fixture executable {name}\n");
        repo.write(&format!("bin/{name}"), bytes.as_bytes());
        executables.insert(name.to_owned(), json!(sha256_hex(bytes.as_bytes())));
    }
    let lock_path = repo.path(&lock_relative("hermes"));
    let package_digest = sha256_prefixed(&std::fs::read(&archive).expect("read archive"));
    let lock_digest = sha256_prefixed(&std::fs::read(&lock_path).expect("read lock"));
    let report = json!({
        "schema": 1,
        "runtime": "hermes",
        "suite_version": SUITE_VERSION,
        "package_digest": package_digest,
        "upstream_version": HERMES_RELEASE,
        "executables": executables,
    });
    repo.write(
        "report.json",
        &serde_json::to_vec_pretty(&report).expect("report"),
    );
    let matrix = repo.path("compat/matrix.json");
    for target in TARGETS {
        let attestation = repo.path(&format!("attestation-{target}.json"));
        let inputs: [&dyn AsRef<std::ffi::OsStr>; 12] = [
            &"--bin-dir",
            &bin_dir,
            &"--package",
            &archive,
            &"--lock",
            &lock_path,
            &"--matrix",
            &matrix,
            &"--commit",
            &COMMIT,
            &"--target",
            &target,
        ];
        let mut attest: Vec<&dyn AsRef<std::ffi::OsStr>> = vec![
            &"compat",
            &"attest",
            &"--report",
            &"report.json",
            &"--output",
            &attestation,
        ];
        attest.extend(inputs);
        assert_eq!(
            repo.xtask_ok(&attest),
            format!("compat attest ok: hermes on {target} at suite {SUITE_VERSION}\n")
        );
        let document: Value =
            serde_json::from_slice(&std::fs::read(&attestation).expect("read attestation"))
                .expect("attestation JSON");
        assert_eq!(document["package_id"], "pohunek.runtime.hermes");
        assert_eq!(document["runtime"], "hermes");
        assert_eq!(document["upstream_version"], HERMES_RELEASE);
        assert_eq!(document["package_digest"], package_digest.as_str());
        assert_eq!(document["upstream_lock_digest"], lock_digest.as_str());
        assert_eq!(document["target"], target);

        let mut verify: Vec<&dyn AsRef<std::ffi::OsStr>> =
            vec![&"compat", &"verify", &"--attestation", &attestation];
        verify.extend(inputs);
        assert_eq!(
            repo.xtask_ok(&verify),
            format!("compat verify ok: hermes on {target} at suite {SUITE_VERSION}\n")
        );
    }
}
