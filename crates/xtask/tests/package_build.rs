//! Scenarios for `cargo xtask package build --evidence-dir` driven through the
//! real binary over a real Git repository.
//!
//! Each scenario commits a fixture repository with the layout the builder
//! reads (`runtime-packages/<p>/`, `compat/<p>/compatibility-lock.json`,
//! `compat/matrix.json`) and runs the `xtask` binary on it. The expected
//! documents are written out below from the fixture files: the per-file
//! SHA-256 values, sizes and the Git tree id were computed from the fixture
//! bytes with `sha256sum`, `wc -c` and `git rev-parse`, never by the code under
//! test. The archive digest is recomputed here from the archive bytes and the
//! commit id is read from Git, because both are only defined by those bytes.
//!
//! - the evidence holds exactly the four documents with the expected content;
//! - a non-npm lock `upstream` object is copied whole with sorted keys, and a
//!   lock without `upstream.release` is refused;
//! - a git failure is reported in the command's own context;
//! - bytes that differ from `HEAD` behind a clean `git status` (skip-worktree,
//!   assume-unchanged, `core.autocrlf`, `core.fileMode=false`) are refused, in
//!   SHA-1 and SHA-256 repositories;
//! - an archive output that is a directory or lacks its directory, and an
//!   evidence directory without a parent, are refused before any write;
//! - two clones of one commit, built into different directories, produce
//!   byte-identical archives and evidence;
//! - a modified, untracked or ignored package file, a modified lock, a missing
//!   lock, a lock of another runtime, an evidence directory inside the package,
//!   an evidence path that is a file or holds a symlinked evidence file, and a
//!   package directory outside `runtime-packages/` are refused with nothing
//!   written.

// Rust guideline compliant 2026-10-12

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// Descriptor of the fixture's Pi package.
const PI_DESCRIPTOR: &str = "schema = 1\nid = \"pohunek.runtime.pi\"\nversion = \"1.0.0\"\nruntime_api = 1\n\n[runtime]\nid = \"pi\"\nname = \"Pi\"\nprogram = \"pi\"\n";

/// Detection manifest of the fixture's Pi package.
const PI_DETECT: &str = "[detect]\nidle = \"ready\"\n";

/// Executable helper of the fixture's Pi package (committed with mode 0755).
const PI_PROBE: &str = "#!/bin/sh\nexec pi --version\n";

/// Compatibility lock of the fixture's Pi package.
const PI_LOCK: &str = "{\n  \"schema\": 1,\n  \"package\": \"pohunek.runtime.pi\",\n  \"runtime\": \"pi\",\n  \"upstream\": {\n    \"npm\": \"@example/pi-agent\",\n    \"release\": \"1.0.2\",\n    \"integrity\": \"sha512-cGk=\"\n  },\n  \"supported\": {\n    \"min\": \"1.0.0\",\n    \"below\": \"1.1.0\"\n  }\n}\n";

/// Descriptor of a second package, whose matrix rows the Pi vector excludes.
const CODEX_DESCRIPTOR: &str = "schema = 1\nid = \"pohunek.runtime.codex\"\nversion = \"1.0.0\"\nruntime_api = 1\n\n[runtime]\nid = \"codex\"\nname = \"Codex\"\nprogram = \"codex\"\n";

/// The fixture matrix: two runtimes on two targets, suite version 3.
const MATRIX: &str = r#"{
  "schema": 1,
  "suite_version": 3,
  "rows": [
    {"runtime": "codex", "target": "x86_64-unknown-linux-gnu"},
    {"runtime": "codex", "target": "x86_64-unknown-linux-musl"},
    {"runtime": "pi", "target": "x86_64-unknown-linux-gnu"},
    {"runtime": "pi", "target": "x86_64-unknown-linux-musl"}
  ]
}
"#;

/// `sha256sum compat/pi/compatibility-lock.json` of [`PI_LOCK`].
const PI_LOCK_DIGEST: &str =
    "sha256:42c1c0188df0f6e32def713889a436655b8976ec58e5174714b2e2dbf4931a3a";

/// `git rev-parse HEAD:runtime-packages/pi` of the committed fixture package.
const PI_TREE: &str = "79e9c08abe5c6b5e7bd9ef2c6ca32a1574cc3c20";

/// The workspace version, which `xtask` reports as its own.
const XTASK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Archive file name every scenario builds.
const ARCHIVE: &str = "pi.tar.zst";

/// The evidence files of a Pi build, ascending.
const EVIDENCE_FILES: [&str; 4] = [
    "pi.compat-vector.json",
    "pi.manifest.json",
    "pi.provenance-input.json",
    "pi.tar.zst.sha256",
];

/// The archive entries of the Pi package (`sha256sum`, `wc -c`, mode).
fn pi_entries() -> Value {
    json!([
        {
            "executable": true,
            "path": "bin/probe.sh",
            "sha256": "2e99e080727af5233a24308be3ce0320afa29132edbf4aa531e2854a68afe439",
            "size": 28
        },
        {
            "executable": false,
            "path": "detect.toml",
            "sha256": "7073ad3485ec27b1072c2ce2fc8ee222ccf52fea405d2b8ea534fb5d8b37aa13",
            "size": 24
        },
        {
            "executable": false,
            "path": "runtime.toml",
            "sha256": "d6ed7b446f28f4c859756e395359eb2b307c00b7e6a6696454507170ee71111a",
            "size": 119
        }
    ])
}

/// A committed fixture repository in a hermetic environment.
struct Repo {
    env: TestEnv,
}

impl Repo {
    fn new() -> Self {
        Self::with_lock(PI_LOCK)
    }

    /// The fixture with `lock` as the Pi compatibility lock.
    fn with_lock(lock: &str) -> Self {
        Self::init(lock, "sha1")
    }

    /// The fixture in a repository of the object format `format`.
    fn init(lock: &str, format: &str) -> Self {
        let repo = Self {
            env: TestEnv::new().expect("hermetic test environment"),
        };
        repo.git(&[
            "init",
            "--quiet",
            "--initial-branch=main",
            &format!("--object-format={format}"),
        ]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo.git(&["config", "user.name", "test"]);
        // Marks the fixture as a checkout, which the xtask binary requires.
        repo.write("crates/xtask/Cargo.toml", "");
        repo.write("runtime-packages/pi/runtime.toml", PI_DESCRIPTOR);
        repo.write("runtime-packages/pi/detect.toml", PI_DETECT);
        repo.write("runtime-packages/pi/bin/probe.sh", PI_PROBE);
        std::fs::set_permissions(
            repo.dir().join("runtime-packages/pi/bin/probe.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod probe");
        repo.write("runtime-packages/codex/runtime.toml", CODEX_DESCRIPTOR);
        repo.write("compat/pi/compatibility-lock.json", lock);
        repo.write("compat/matrix.json", MATRIX);
        repo.write(".gitignore", "*.log\n");
        repo.commit("fixture");
        repo
    }

    fn dir(&self) -> &Path {
        self.env.cwd()
    }

    /// A directory outside the repository for build outputs.
    fn out(&self, name: &str) -> PathBuf {
        self.env.root().join(name)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.dir().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, content).expect("write file");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    /// Runs git in `dir` with system and global configuration disabled, so
    /// the developer's identity, hooks, excludes and signing never apply.
    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = self
            .env
            .git_command()
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 git output")
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(self.dir(), args)
    }

    /// Runs the real `xtask package build <package> -o <archive> --evidence-dir <evidence>`.
    fn build_in(&self, dir: &Path, package: &str, archive: &Path, evidence: &Path) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .current_dir(dir)
            .args(["package", "build", package, "--output"])
            .arg(archive)
            .arg("--evidence-dir")
            .arg(evidence)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run xtask")
    }

    fn build(&self, archive: &Path, evidence: &Path) -> Output {
        self.build_in(self.dir(), "runtime-packages/pi", archive, evidence)
    }

    /// Requires the build to be refused with `reason` on stderr and to leave
    /// neither the archive nor the evidence directory behind.
    fn assert_refused(&self, package: &str, evidence: &Path, reason: &str) {
        let archive = self.out("refused").join(ARCHIVE);
        std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
        let output = self.build_in(self.dir(), package, &archive, evidence);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "the build must be refused");
        assert!(stderr.contains(reason), "expected {reason:?} in: {stderr}");
        assert!(!archive.exists(), "a refused build wrote the archive");
        assert!(!evidence.exists(), "a refused build wrote evidence");
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").expect("write to a String");
            text
        })
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read evidence")).expect("evidence JSON")
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list evidence")
        .map(|entry| {
            entry
                .expect("evidence entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn the_evidence_describes_the_archive_the_source_and_the_lock() {
    let repo = Repo::new();
    let archive = repo.out("archives").join(ARCHIVE);
    std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
    let evidence = repo.out("evidence");
    let output = repo.build(&archive, &evidence);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Only the renamed archive: no staging file is left next to it.
    assert_eq!(file_names(archive.parent().expect("parent")), [ARCHIVE]);
    let archive_hex = sha256_hex(&std::fs::read(&archive).expect("read archive"));
    let digest = format!("sha256:{archive_hex}");
    let commit = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(file_names(&evidence), EVIDENCE_FILES);

    assert_eq!(
        std::fs::read_to_string(evidence.join("pi.tar.zst.sha256")).expect("read checksum"),
        format!("{archive_hex}  pi.tar.zst\n")
    );
    assert_eq!(
        read_json(&evidence.join("pi.manifest.json")),
        json!({
            "digest": digest,
            "entries": pi_entries(),
            "package_id": "pohunek.runtime.pi",
            "runtime_api": 1,
            "runtime_id": "pi",
            "schema": 1,
            "version": "1.0.0"
        })
    );
    assert_eq!(
        read_json(&evidence.join("pi.provenance-input.json")),
        json!({
            "builder": {
                "command": "cargo xtask package build",
                "xtask_version": XTASK_VERSION
            },
            "materials": {
                "entries": pi_entries(),
                "upstream": {
                    "integrity": "sha512-cGk=",
                    "npm": "@example/pi-agent",
                    "release": "1.0.2"
                }
            },
            "schema": 1,
            "source": {
                "commit": commit,
                "path": "runtime-packages/pi",
                "tree": PI_TREE
            },
            "subject": {
                "digest": digest,
                "name": "pi.tar.zst"
            }
        })
    );
    assert_eq!(
        read_json(&evidence.join("pi.compat-vector.json")),
        json!({
            "package_digest": digest,
            "package_id": "pohunek.runtime.pi",
            "runtime": "pi",
            "schema": 1,
            "suite_version": 3,
            "supported": {"below": "1.1.0", "min": "1.0.0"},
            "targets": ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
            "upstream": {
                "integrity": "sha512-cGk=",
                "npm": "@example/pi-agent",
                "release": "1.0.2"
            },
            "upstream_lock_digest": PI_LOCK_DIGEST,
            "upstream_version": "1.0.2"
        })
    );
    // Canonical rendering: two-space indentation, one trailing newline.
    let manifest = std::fs::read_to_string(evidence.join("pi.manifest.json")).expect("read");
    assert!(manifest.starts_with("{\n  \"digest\": "), "{manifest}");
    assert!(manifest.ends_with("}\n") && !manifest.ends_with("}\n\n"));
}

/// A lock whose upstream is a provisioned runtime, not an npm package. Its
/// keys are out of order on purpose, at the top and in a nested object.
const PROVISIONER_LOCK: &str = r#"{
  "schema": 1,
  "package": "pohunek.runtime.pi",
  "runtime": "pi",
  "upstream": {
    "release": "1.2.3",
    "kind": "provisioner",
    "script": "scripts/x",
    "source": {"sha256": "ab12", "format": "tar"},
    "pinned": true,
    "attempts": 2
  },
  "supported": {
    "min": "1.2.0",
    "below": "1.3.0"
  }
}
"#;

/// `sha256sum compat/pi/compatibility-lock.json` of [`PROVISIONER_LOCK`].
const PROVISIONER_LOCK_DIGEST: &str =
    "sha256:3f37fc6e658f836d329a75e2887c7c946e5184aeea7e0616eba5e843578a7fa9";

#[test]
fn a_non_npm_upstream_is_copied_whole_with_sorted_keys() {
    let repo = Repo::with_lock(PROVISIONER_LOCK);
    let archive = repo.out("archives").join(ARCHIVE);
    std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
    let evidence = repo.out("evidence");
    let output = repo.build(&archive, &evidence);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let digest = format!(
        "sha256:{}",
        sha256_hex(&std::fs::read(&archive).expect("read archive"))
    );

    // The exact bytes: sorted keys at every level, two-space indentation.
    let upstream = r#"{
    "attempts": 2,
    "kind": "provisioner",
    "pinned": true,
    "release": "1.2.3",
    "script": "scripts/x",
    "source": {
      "format": "tar",
      "sha256": "ab12"
    }
  }"#;
    assert_eq!(
        std::fs::read_to_string(evidence.join("pi.compat-vector.json")).expect("read vector"),
        format!(
            r#"{{
  "package_digest": "{digest}",
  "package_id": "pohunek.runtime.pi",
  "runtime": "pi",
  "schema": 1,
  "suite_version": 3,
  "supported": {{
    "below": "1.3.0",
    "min": "1.2.0"
  }},
  "targets": [
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl"
  ],
  "upstream": {upstream},
  "upstream_lock_digest": "{PROVISIONER_LOCK_DIGEST}",
  "upstream_version": "1.2.3"
}}
"#
        )
    );
    let provenance =
        std::fs::read_to_string(evidence.join("pi.provenance-input.json")).expect("read");
    assert_eq!(
        read_json(&evidence.join("pi.provenance-input.json"))["materials"]["upstream"],
        json!({
            "attempts": 2,
            "kind": "provisioner",
            "pinned": true,
            "release": "1.2.3",
            "script": "scripts/x",
            "source": {"format": "tar", "sha256": "ab12"}
        })
    );
    // One level deeper in the provenance input (`materials.upstream`).
    let nested = upstream.replace('\n', "\n  ");
    assert!(
        provenance.contains(&format!("\"upstream\": {nested}\n")),
        "{provenance}"
    );
}

#[test]
fn a_lock_without_an_upstream_release_is_refused_with_nothing_written() {
    let repo = Repo::with_lock(&PROVISIONER_LOCK.replace("\"release\": \"1.2.3\",", ""));
    repo.assert_refused(
        "runtime-packages/pi",
        &repo.out("evidence"),
        "field `upstream_lock` is malformed",
    );
}

#[test]
fn a_package_outside_a_git_work_tree_is_refused_in_the_command_context() {
    let env = TestEnv::new().expect("hermetic test environment");
    let package = env.cwd().join("runtime-packages/pi");
    std::fs::create_dir_all(&package).expect("mkdir");
    std::fs::write(package.join("runtime.toml"), PI_DESCRIPTOR).expect("write");
    // Marks the directory as a checkout, which the xtask binary requires.
    std::fs::create_dir_all(env.cwd().join("crates/xtask")).expect("mkdir xtask");
    std::fs::write(env.cwd().join("crates/xtask/Cargo.toml"), "").expect("write marker");
    let archive = env.root().join(ARCHIVE);
    let evidence = env.root().join("evidence");
    let output = env
        .command(pohunek_test_support::bin_exe("xtask"))
        .args(["package", "build", "runtime-packages/pi", "--output"])
        .arg(&archive)
        .arg("--evidence-dir")
        .arg(&evidence)
        .env("GIT_CEILING_DIRECTORIES", env.root())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run xtask");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a package outside Git must be refused"
    );
    assert!(
        stderr.contains("package evidence: `git rev-parse --show-toplevel` failed"),
        "{stderr}"
    );
    assert!(!stderr.contains("affected:"), "{stderr}");
    assert!(!archive.exists() && !evidence.exists());
}

#[test]
fn two_clones_of_one_commit_produce_byte_identical_evidence() {
    let repo = Repo::new();
    let clone = repo.out("clone");
    let source = repo.dir().to_str().expect("utf-8 path").to_owned();
    let target = clone.to_str().expect("utf-8 path").to_owned();
    repo.git_in(repo.env.root(), &["clone", "--quiet", &source, &target]);

    let mut builds = Vec::new();
    for (dir, name) in [(repo.dir().to_path_buf(), "first"), (clone, "second")] {
        let archive = repo.out(&format!("{name}-archives")).join(ARCHIVE);
        std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
        let evidence = repo.out(&format!("{name}-evidence"));
        let output = repo.build_in(&dir, "runtime-packages/pi", &archive, &evidence);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        builds.push((archive, evidence));
    }
    let [(first_archive, first), (second_archive, second)] = &*builds else {
        panic!("two builds");
    };
    assert_eq!(
        std::fs::read(first_archive).expect("read"),
        std::fs::read(second_archive).expect("read")
    );
    assert_eq!(file_names(first), EVIDENCE_FILES);
    assert_eq!(file_names(second), EVIDENCE_FILES);
    for name in EVIDENCE_FILES {
        assert_eq!(
            std::fs::read(first.join(name)).expect("read"),
            std::fs::read(second.join(name)).expect("read"),
            "{name} differs between the clones"
        );
    }
}

/// A named change that leaves the fixture tree different from `HEAD`.
type Case = (&'static str, fn(&Repo));

#[test]
fn a_tree_that_differs_from_head_is_refused_with_nothing_written() {
    let cases: [Case; 4] = [
        ("modified package file", |repo| {
            repo.write("runtime-packages/pi/detect.toml", "[detect]\n");
        }),
        ("untracked package file", |repo| {
            repo.write("runtime-packages/pi/extra.toml", "x = 1\n");
        }),
        ("ignored package file", |repo| {
            repo.write("runtime-packages/pi/build.log", "noise\n");
        }),
        ("modified lock", |repo| {
            repo.write(
                "compat/pi/compatibility-lock.json",
                &PI_LOCK.replace("1.0.2", "1.0.3"),
            );
        }),
    ];
    for (case, change) in cases {
        let repo = Repo::new();
        change(&repo);
        let evidence = repo.out("evidence");
        let archive = repo.out("refused").join(ARCHIVE);
        std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
        let output = repo.build(&archive, &evidence);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{case}: the build must be refused"
        );
        assert!(stderr.contains("needs a clean tree"), "{case}: {stderr}");
        assert!(!archive.exists(), "{case}: the archive was written");
        assert!(!evidence.exists(), "{case}: evidence was written");
    }
}

/// Applies `change`, which `git status` must not report, to a fresh fixture
/// and requires the build to be refused with `reason` and nothing written.
fn assert_hidden_change_refused(change: fn(&Repo), reason: &str) {
    let repo = Repo::new();
    change(&repo);
    assert_eq!(
        repo.git(&[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored"
        ]),
        "",
        "the change must be invisible to git status"
    );
    repo.assert_refused("runtime-packages/pi", &repo.out("evidence"), reason);
}

#[test]
fn a_deleted_skip_worktree_package_file_is_refused() {
    assert_hidden_change_refused(
        |repo| {
            repo.git(&[
                "update-index",
                "--skip-worktree",
                "runtime-packages/pi/detect.toml",
            ]);
            std::fs::remove_file(repo.dir().join("runtime-packages/pi/detect.toml"))
                .expect("remove");
        },
        "`runtime-packages/pi/detect.toml` differs from HEAD (absent from the work tree)",
    );
}

#[test]
fn a_crlf_checkout_under_core_autocrlf_is_refused() {
    assert_hidden_change_refused(
        |repo| {
            // A checkout under `core.autocrlf` writes CRLF and records the
            // checked-out stat, so the status stays clean.
            repo.git(&["config", "core.autocrlf", "true"]);
            std::fs::remove_file(repo.dir().join("runtime-packages/pi/detect.toml"))
                .expect("remove");
            repo.git(&["checkout", "--", "runtime-packages/pi/detect.toml"]);
            assert_eq!(
                std::fs::read_to_string(repo.dir().join("runtime-packages/pi/detect.toml"))
                    .expect("read"),
                PI_DETECT.replace('\n', "\r\n")
            );
        },
        "`runtime-packages/pi/detect.toml` differs from HEAD (content)",
    );
}

#[test]
fn a_chmod_hidden_by_core_file_mode_false_is_refused() {
    assert_hidden_change_refused(
        |repo| {
            repo.git(&["config", "core.fileMode", "false"]);
            std::fs::set_permissions(
                repo.dir().join("runtime-packages/pi/detect.toml"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
        },
        "`runtime-packages/pi/detect.toml` differs from HEAD (file mode)",
    );
}

#[test]
fn an_assume_unchanged_package_file_is_refused() {
    assert_hidden_change_refused(
        |repo| {
            repo.git(&[
                "update-index",
                "--assume-unchanged",
                "runtime-packages/pi/detect.toml",
            ]);
            repo.write(
                "runtime-packages/pi/detect.toml",
                "[detect]\nidle = \"other\"\n",
            );
        },
        "`runtime-packages/pi/detect.toml` differs from HEAD (content)",
    );
}

#[test]
fn a_skip_worktree_lock_is_refused() {
    assert_hidden_change_refused(
        |repo| {
            repo.git(&[
                "update-index",
                "--skip-worktree",
                "compat/pi/compatibility-lock.json",
            ]);
            repo.write(
                "compat/pi/compatibility-lock.json",
                &PI_LOCK.replace("sha512-cGk=", "sha512-cXk="),
            );
        },
        "`compat/pi/compatibility-lock.json` differs from HEAD (content)",
    );
}

#[test]
fn a_skip_worktree_matrix_is_refused() {
    assert_hidden_change_refused(
        |repo| {
            repo.git(&["update-index", "--skip-worktree", "compat/matrix.json"]);
            repo.write(
                "compat/matrix.json",
                &MATRIX.replace("\"suite_version\": 3", "\"suite_version\": 4"),
            );
        },
        "`compat/matrix.json` differs from HEAD (content)",
    );
}

#[test]
fn a_sha256_repository_is_verified_with_its_own_object_ids() {
    let repo = Repo::init(PI_LOCK, "sha256");
    let archive = repo.out("archives").join(ARCHIVE);
    std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
    let evidence = repo.out("evidence");
    let output = repo.build(&archive, &evidence);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let source = &read_json(&evidence.join("pi.provenance-input.json"))["source"];
    assert_eq!(
        source,
        &json!({
            "commit": repo.git(&["rev-parse", "HEAD"]).trim(),
            "path": "runtime-packages/pi",
            "tree": repo.git(&["rev-parse", "HEAD:runtime-packages/pi"]).trim()
        })
    );
    assert_eq!(source["tree"].as_str().map(str::len), Some(64));

    // A changed byte behind assume-unchanged is still caught with SHA-256 ids.
    repo.git(&[
        "update-index",
        "--assume-unchanged",
        "runtime-packages/pi/runtime.toml",
    ]);
    repo.write(
        "runtime-packages/pi/runtime.toml",
        &PI_DESCRIPTOR.replace("Pi", "PI"),
    );
    repo.assert_refused(
        "runtime-packages/pi",
        &repo.out("second-evidence"),
        "`runtime-packages/pi/runtime.toml` differs from HEAD (content)",
    );
}

#[test]
fn an_output_that_cannot_be_written_is_refused_before_anything_is_written() {
    let repo = Repo::new();
    let directory = repo.out("archive-is-a-directory");
    std::fs::create_dir(&directory).expect("mkdir");
    let evidence = repo.out("evidence");
    let output = repo.build(&directory, &evidence);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a directory as archive output");
    assert!(
        stderr.contains("exists and is not a regular file"),
        "{stderr}"
    );
    assert!(!evidence.exists(), "evidence directory was created");
    assert_eq!(file_names(&directory), Vec::<String>::new());

    let missing = repo.out("missing-parent").join(ARCHIVE);
    let output = repo.build(&missing, &evidence);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an archive output without a directory"
    );
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(!evidence.exists(), "evidence directory was created");

    let archive = repo.out("archives").join(ARCHIVE);
    std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
    let nested = repo.out("missing-parent").join("evidence");
    let output = repo.build(&archive, &nested);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an evidence directory without a parent"
    );
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(!archive.exists(), "the archive was written");
    assert!(!repo.out("missing-parent").exists());
}

#[test]
fn a_missing_lock_is_refused_with_nothing_written() {
    let repo = Repo::new();
    repo.git(&["rm", "--quiet", "compat/pi/compatibility-lock.json"]);
    repo.commit("drop the lock");
    repo.assert_refused(
        "runtime-packages/pi",
        &repo.out("evidence"),
        "field `lock.pi` is missing",
    );
}

#[test]
fn a_lock_of_another_runtime_is_refused_with_nothing_written() {
    let repo = Repo::new();
    repo.write(
        "compat/pi/compatibility-lock.json",
        &PI_LOCK.replace("\"runtime\": \"pi\"", "\"runtime\": \"codex\""),
    );
    repo.commit("lock names another runtime");
    repo.assert_refused(
        "runtime-packages/pi",
        &repo.out("evidence"),
        "field `lock.pi.runtime` does not match",
    );
}

#[test]
fn an_evidence_directory_inside_the_package_is_refused_with_nothing_written() {
    let repo = Repo::new();
    let inside = repo.dir().join("runtime-packages/pi/evidence");
    repo.assert_refused(
        "runtime-packages/pi",
        &inside,
        "is inside the package directory",
    );
    // An existing directory below the package is refused the same way and
    // stays empty.
    let existing = repo.dir().join("runtime-packages/pi/bin");
    let output = repo.build(&repo.out("refused").join(ARCHIVE), &existing);
    assert!(!output.status.success());
    assert_eq!(file_names(&existing), ["probe.sh"]);
}

#[test]
fn an_evidence_target_that_cannot_be_written_is_refused_before_the_archive() {
    let repo = Repo::new();
    let file = repo.out("evidence-file");
    std::fs::write(&file, "not a directory\n").expect("write");
    let archive = repo.out("refused").join(ARCHIVE);
    std::fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
    let output = repo.build(&archive, &file);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a file as evidence directory");
    assert!(stderr.contains("is not a directory"), "{stderr}");
    assert!(!archive.exists(), "the archive was written");

    let evidence = repo.out("evidence");
    std::fs::create_dir(&evidence).expect("mkdir");
    let elsewhere = repo.out("elsewhere.json");
    std::os::unix::fs::symlink(&elsewhere, evidence.join("pi.manifest.json")).expect("symlink");
    let output = repo.build(&archive, &evidence);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a symlinked evidence file");
    assert!(stderr.contains("is a symbolic link"), "{stderr}");
    assert!(!archive.exists(), "the archive was written");
    assert!(!elsewhere.exists(), "the link was written through");
    assert_eq!(file_names(&evidence), ["pi.manifest.json"]);
}

#[test]
fn a_package_outside_the_runtime_packages_directory_is_refused() {
    let repo = Repo::new();
    for (path, content) in [
        ("other/pi/runtime.toml", PI_DESCRIPTOR),
        ("other/pi/detect.toml", PI_DETECT),
    ] {
        repo.write(path, content);
    }
    repo.commit("a copy outside runtime-packages");
    repo.assert_refused(
        "other/pi",
        &repo.out("evidence"),
        "needs a directory `runtime-packages/<runtime>`",
    );
}
