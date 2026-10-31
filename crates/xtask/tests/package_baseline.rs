//! Scenarios for `cargo xtask package baseline` and `cargo xtask package bump`
//! driven through the real binary.
//!
//! Each scenario builds a Git repository with the layout the verifier reads
//! (`runtime-packages/<p>/`, `compat/<p>/` with a lock, an npm lock and a
//! screen, `compat/matrix.json`) and publishes release `1.2.3` of it the way
//! the release pipeline does, with the real `xtask` subcommands: a throwaway
//! owner-private signing key (`catalog public-key`, `catalog anchor`), the
//! package archives (`package build`), hand-written attestations and the
//! signed catalog (`catalog build`, `catalog sign`). The scenario then changes
//! one input and runs `package baseline --json` or `package bump --json` in
//! the repository.
//!
//! Expected verdicts are written out from the verification rules, never
//! computed by the code under test. A verified digest is the SHA-256 of the
//! published archive file, hashed here.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// Release the fixture publishes.
const VERSION: &str = "1.2.3";

/// Catalog sequence of release `1.2.3`: `major << 40 | minor << 20 | patch`,
/// the ordering the release assembler documents.
const SEQUENCE: u64 = 1_099_513_724_931;

/// Official packages of the fixture repository, ascending.
const PACKAGES: [&str; 3] = ["claude", "codex", "pi"];

const GNU: &str = "x86_64-unknown-linux-gnu";
const MUSL: &str = "x86_64-unknown-linux-musl";

/// Binary-set digests of the fixture release, one per target.
const BINARY_SETS: [(&str, &str); 2] = [
    (
        GNU,
        "sha256:1111111111111111111111111111111111111111111111111111111111111111",
    ),
    (
        MUSL,
        "sha256:2222222222222222222222222222222222222222222222222222222222222222",
    ),
];

/// End of every signing window and of the catalog validity: 2100-01-01.
const WINDOW_END: u64 = 4_102_444_800;

/// A commit id that no fixture repository contains.
const FOREIGN_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Suite version of the fixture matrix.
const SUITE_VERSION: u32 = 1;

/// Directories under the environment root, beside the repository checkout:
/// keys and anchor, the published release, and catalog build inputs.
const KEYS_DIR: &str = "fixture-keys";
const RELEASE_DIR: &str = "fixture-release";
const SCRATCH_DIR: &str = "fixture-scratch";

/// Exit status of an `xtask` failure that is not a delegated command's own
/// exit; CI treats it as every package missing.
const XTASK_FAILURE: i32 = 2;

/// Upstream release every fixture lock pins.
const UPSTREAM_RELEASE: &str = "1.0.2";

fn descriptor(name: &str, version: &str) -> String {
    format!(
        "schema = 1\nid = \"pohunek.runtime.{name}\"\nversion = \"{version}\"\nruntime_api = 1\n\n[runtime]\nid = \"{name}\"\n"
    )
}

fn lock(name: &str, verified_with: &str) -> String {
    format!(
        "{{\n  \"schema\": 1,\n  \"package\": \"pohunek.runtime.{name}\",\n  \"runtime\": \"{name}\",\n  \"upstream\": {{\"release\": \"{UPSTREAM_RELEASE}\"}},\n  \"supported\": {{\"min\": \"1.0.0\", \"below\": \"1.1.0\"}},\n  \"verified_with\": \"{verified_with}\"\n}}\n"
    )
}

fn matrix(packages: &[&str], suite_version: u32) -> String {
    let rows: Vec<Value> = packages
        .iter()
        .flat_map(|name| {
            [GNU, MUSL]
                .into_iter()
                .map(move |target| json!({ "runtime": name, "target": target }))
        })
        .collect();
    let value = json!({ "schema": 1, "suite_version": suite_version, "rows": rows });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&value).expect("matrix json")
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// What one publication of the fixture release looks like.
struct Release<'a> {
    version: &'a str,
    sequence: u64,
    /// Release commit; the repository's `HEAD` when `None`.
    commit: Option<&'a str>,
    /// Targets each package is attested on.
    targets: &'a [&'a str],
    /// Unix second from which the catalog is no longer acceptable.
    expires_at: u64,
    /// How the catalog body is signed.
    signing: Signing,
    /// Changes the attestation of `(package, target)` before it is written and
    /// its digest enters the catalog.
    edit: fn(&str, &str, &mut Value),
}

/// How [`Fixture::publish`] signs the catalog body.
#[derive(Clone, Copy)]
enum Signing {
    /// `catalog sign`, which refuses a body that is invalid now.
    Tool,
    /// `package::sign_catalog` in this process, for a body `catalog sign`
    /// refuses, such as an expired one.
    InProcess,
}

/// The attestation edit that changes nothing.
fn unedited(_package: &str, _target: &str, _document: &mut Value) {}

impl Default for Release<'_> {
    fn default() -> Self {
        Self {
            version: VERSION,
            sequence: SEQUENCE,
            commit: None,
            targets: &[GNU, MUSL],
            expires_at: WINDOW_END,
            signing: Signing::Tool,
            edit: unedited,
        }
    }
}

/// A signing key in an owner-private file, with its id and public key file.
struct Key {
    file: PathBuf,
    /// The key file's seed, for signing what `catalog sign` refuses.
    seed: [u8; 32],
    id: String,
    public: PathBuf,
}

/// Fixture repository plus the release published from it.
struct Fixture {
    env: TestEnv,
    key: Key,
    anchor: PathBuf,
    /// Published archive digest per package.
    released: BTreeMap<String, String>,
}

impl Fixture {
    /// A repository holding the three packages, released as `1.2.3`.
    fn new() -> Self {
        let mut fixture = Self::unreleased();
        fixture.publish(&Release::default());
        fixture
    }

    /// A repository holding the three packages and a trust anchor for a fresh
    /// key, with nothing published yet.
    fn unreleased() -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        for dir in [KEYS_DIR, RELEASE_DIR, SCRATCH_DIR] {
            std::fs::create_dir(env.root().join(dir)).expect("create fixture dir");
        }
        let mut fixture = Self {
            key: Key {
                file: PathBuf::new(),
                seed: [0; 32],
                id: String::new(),
                public: PathBuf::new(),
            },
            anchor: env.root().join(KEYS_DIR).join("anchor.json"),
            env,
            released: BTreeMap::new(),
        };
        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture.git(&["config", "user.email", "test@example.invalid"]);
        fixture.git(&["config", "user.name", "test"]);
        // Marks the fixture as a checkout, which the xtask binary requires.
        fixture.write("crates/xtask/Cargo.toml", "");
        fixture.write("README.md", "# fixture\n");
        fixture.write("compat/matrix.json", &matrix(&PACKAGES, SUITE_VERSION));
        for name in PACKAGES {
            fixture.add_package(name);
        }
        fixture.commit("release");
        fixture.key = fixture.new_key("signer");
        fixture.write_anchor(&fixture.key.public, 1, WINDOW_END);
        fixture
    }

    fn repo(&self) -> &Path {
        self.env.cwd()
    }

    fn release_dir(&self) -> PathBuf {
        self.env.root().join(RELEASE_DIR)
    }

    fn catalog(&self) -> PathBuf {
        self.release_dir().join("runtime-catalog.json")
    }

    fn work(&self, name: &str) -> PathBuf {
        self.env.root().join(SCRATCH_DIR).join(name)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.repo().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, content).expect("write file");
    }

    fn add_package(&self, name: &str) {
        self.write(
            &format!("runtime-packages/{name}/runtime.toml"),
            &descriptor(name, "1.0.0"),
        );
        self.write(
            &format!("runtime-packages/{name}/detect.toml"),
            "[detect]\n",
        );
        self.write(
            &format!("compat/{name}/compatibility-lock.json"),
            &lock(name, "fixture run"),
        );
        self.write(
            &format!("compat/{name}/npm/package-lock.json"),
            "{\n  \"lockfileVersion\": 3\n}\n",
        );
        self.write(&format!("compat/{name}/screens/idle.txt"), "idle\n");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    fn head(&self) -> String {
        let output = self.run_git(&["rev-parse", "HEAD"]);
        String::from_utf8(output.stdout)
            .expect("utf-8 commit id")
            .trim()
            .to_owned()
    }

    /// Runs git with system and global configuration disabled, so the
    /// developer's identity, hooks and signing settings never apply.
    fn run_git(&self, args: &[&str]) -> Output {
        let output = self
            .env
            .git_command()
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
        output
    }

    fn git(&self, args: &[&str]) {
        self.run_git(args);
    }

    /// Runs the real `xtask` in the repository.
    fn xtask(&self, args: &[&dyn AsRef<std::ffi::OsStr>]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run xtask")
    }

    /// Runs `xtask` and requires success; returns stdout.
    fn xtask_ok(&self, args: &[&dyn AsRef<std::ffi::OsStr>]) -> String {
        let output = self.xtask(args);
        assert!(
            output.status.success(),
            "xtask failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }

    /// A fresh signing key: 32 random bytes as 64 hex characters in a 0600
    /// file, with the key id and public key the real tool derives from it.
    fn new_key(&self, name: &str) -> Key {
        let mut seed = [0_u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut seed))
            .expect("operating system randomness");
        let file = self.env.root().join(KEYS_DIR).join(format!("{name}.key"));
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .expect("create key file");
        handle
            .write_all(format!("{}\n", hex(&seed)).as_bytes())
            .expect("write key file");
        let stdout = self.xtask_ok(&[&"catalog", &"public-key", &"--key-file", &file]);
        let field = |prefix: &str| {
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("`{prefix}` in {stdout:?}"))
                .to_owned()
        };
        let public = self.env.root().join(KEYS_DIR).join(format!("{name}.pub"));
        std::fs::write(&public, format!("{}\n", field("public_key: "))).expect("write pub");
        Key {
            file,
            seed,
            id: field("key_id: "),
            public,
        }
    }

    /// Writes the anchor trusting `public` from `not_before` to `not_after`.
    fn write_anchor(&self, public: &Path, not_before: u64, not_after: u64) {
        self.write_anchor_revoking(public, not_before, not_after, &[]);
    }

    /// Writes the anchor trusting `public` from `not_before` to `not_after`
    /// and revoking the key ids `revoked`.
    fn write_anchor_revoking(
        &self,
        public: &Path,
        not_before: u64,
        not_after: u64,
        revoked: &[&str],
    ) {
        let root = format!("{}:{not_before}:{not_after}", public.display());
        let mut args: Vec<&dyn AsRef<std::ffi::OsStr>> = vec![
            &"catalog",
            &"anchor",
            &"--root",
            &root,
            &"--output",
            &self.anchor,
        ];
        for id in revoked {
            args.push(&"--revoked-key-id");
            args.push(id);
        }
        self.xtask_ok(&args);
    }

    /// Publishes the release of the repository's current state into the
    /// release directory, replacing any earlier publication.
    fn publish(&mut self, release: &Release<'_>) {
        let dir = self.release_dir();
        std::fs::remove_dir_all(&dir).expect("clear release dir");
        std::fs::create_dir(&dir).expect("create release dir");
        let commit = release.commit.map_or_else(|| self.head(), str::to_owned);
        let mut packages = Vec::new();
        self.released.clear();
        for name in PACKAGES {
            let archive = dir.join(format!(
                "pohunek-runtime-{name}-{}.tar.zst",
                release.version
            ));
            self.xtask_ok(&[
                &"package",
                &"build",
                &format!("runtime-packages/{name}"),
                &"--output",
                &archive,
            ]);
            let digest = digest_of(&std::fs::read(&archive).expect("read archive"));
            let lock_bytes = std::fs::read(
                self.repo()
                    .join(format!("compat/{name}/compatibility-lock.json")),
            )
            .expect("read lock");
            let mut attestations = Vec::new();
            for target in release.targets {
                let (_, binary_set) = BINARY_SETS
                    .iter()
                    .find(|(set_target, _)| set_target == target)
                    .expect("binary set of target");
                let mut document = json!({
                    "binary_set_digest": binary_set,
                    "commit": commit,
                    "package_digest": digest,
                    "package_id": format!("pohunek.runtime.{name}"),
                    "platform": target,
                    "runtime": name,
                    "schema": 1,
                    "suite_version": SUITE_VERSION,
                    "target": target,
                    "upstream_lock_digest": digest_of(&lock_bytes),
                    "upstream_version": UPSTREAM_RELEASE,
                });
                (release.edit)(name, target, &mut document);
                let mut bytes = serde_json::to_vec_pretty(&document).expect("attestation");
                bytes.push(b'\n');
                std::fs::write(
                    dir.join(format!("attestation-{name}-{target}.json")),
                    &bytes,
                )
                .expect("write attestation");
                attestations.push(json!({ "platform": target, "digest": digest_of(&bytes) }));
            }
            packages.push(json!({
                "archive": archive,
                "platforms": release.targets,
                "attestations": attestations,
                "core": format!("={}", release.version),
            }));
            self.released.insert(name.to_owned(), digest);
        }
        let binary_sets: Vec<Value> = BINARY_SETS
            .iter()
            .map(|(target, digest)| json!({ "target": target, "digest": digest }))
            .collect();
        let spec = json!({
            "schema_version": 2,
            "sequence": release.sequence,
            "expires_at": release.expires_at,
            "release": {
                "version": release.version,
                "commit": commit,
                "binary_sets": binary_sets,
            },
            "revoked_key_ids": [],
            "revoked_digests": [],
            "packages": packages,
        });
        let spec_path = self.work("spec.json");
        std::fs::write(&spec_path, serde_json::to_vec(&spec).expect("spec")).expect("write spec");
        let unsigned = self.work("unsigned.json");
        self.xtask_ok(&[
            &"catalog",
            &"build",
            &"--spec",
            &spec_path,
            &"--output",
            &unsigned,
        ]);
        self.sign(&unsigned, release.signing);
    }

    /// Signs the unsigned catalog body at `unsigned` into the release directory.
    fn sign(&self, unsigned: &Path, signing: Signing) {
        match signing {
            Signing::Tool => {
                self.xtask_ok(&[
                    &"catalog",
                    &"sign",
                    &"--catalog",
                    &unsigned,
                    &"--key-file",
                    &self.key.file,
                    &"--key-id",
                    &self.key.id,
                    &"--output",
                    &self.catalog(),
                ]);
            }
            Signing::InProcess => {
                let body: package::Catalog =
                    serde_json::from_slice(&std::fs::read(unsigned).expect("read unsigned"))
                        .expect("unsigned catalog");
                let key = ed25519_dalek::SigningKey::from_bytes(&self.key.seed);
                let envelope = package::sign_catalog(body, &key).expect("sign catalog");
                let document = package::catalog_document_bytes(&envelope).expect("catalog bytes");
                std::fs::write(self.catalog(), document).expect("write catalog");
            }
        }
    }

    /// Runs `package baseline --json` for `version`; returns the verdicts and stderr.
    fn baseline_for(&self, version: &str) -> (Value, String) {
        let output = self.baseline_output(version, true);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "baseline failed: {stderr}");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert_eq!(stdout.lines().count(), 1, "one JSON line: {stdout:?}");
        (serde_json::from_str(&stdout).expect("verdict JSON"), stderr)
    }

    fn baseline(&self) -> (Value, String) {
        self.baseline_for(VERSION)
    }

    /// Runs `package bump --json` for [`VERSION`]; returns the exit status and
    /// the one-line report on stdout, which the command prints before it fails.
    fn bump(&self) -> (ExitStatus, Value) {
        self.bump_for(VERSION)
    }

    fn bump_for(&self, version: &str) -> (ExitStatus, Value) {
        let output = self.verify_output("bump", version, true);
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert_eq!(stdout.lines().count(), 1, "one JSON line: {stdout:?}");
        (
            output.status,
            serde_json::from_str(&stdout).expect("gate JSON"),
        )
    }

    fn baseline_output(&self, version: &str, json: bool) -> Output {
        self.verify_output("baseline", version, json)
    }

    /// Runs the `package` subcommand `action` against the published release.
    fn verify_output(&self, action: &str, version: &str, json: bool) -> Output {
        let catalog = self.catalog();
        let release_dir = self.release_dir();
        let mut args: Vec<&dyn AsRef<std::ffi::OsStr>> = vec![
            &"package",
            &action,
            &"--catalog",
            &catalog,
            &"--anchor",
            &self.anchor,
            &"--release-dir",
            &release_dir,
            &"--version",
            &version,
        ];
        if json {
            args.push(&"--json");
        }
        self.xtask(&args)
    }

    /// Replaces the release file `name` with a symbolic link to a copy of
    /// itself outside the release directory: the same bytes, reached through a link.
    fn replace_with_symlink(&self, name: &str) {
        let original = self.release_dir().join(name);
        let copy = self.work(name);
        std::fs::copy(&original, &copy).expect("copy release file");
        std::fs::remove_file(&original).expect("remove release file");
        std::os::unix::fs::symlink(&copy, &original).expect("symlink release file");
    }

    /// Replaces the release file `name` with an empty directory.
    fn replace_with_directory(&self, name: &str) {
        let original = self.release_dir().join(name);
        std::fs::remove_file(&original).expect("remove release file");
        std::fs::create_dir(&original).expect("create directory");
    }

    fn verified(&self, name: &str) -> Value {
        json!({
            "verdict": "verified",
            "reason": null,
            "digest": self.released[name],
            "version_not_bumped": false,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn missing(reason: &str) -> Value {
    json!({
        "verdict": "missing",
        "reason": reason,
        "digest": null,
        "version_not_bumped": false,
    })
}

fn verdicts(version: &str, packages: &[(&str, Value)]) -> Value {
    let packages: serde_json::Map<String, Value> = packages
        .iter()
        .map(|(name, verdict)| ((*name).to_owned(), verdict.clone()))
        .collect();
    json!({ "version": version, "packages": packages })
}

fn all_missing(version: &str, reason: &str) -> Value {
    verdicts(version, &PACKAGES.map(|name| (name, missing(reason))))
}

#[test]
fn an_unchanged_checkout_verifies_every_package() {
    let fixture = Fixture::new();
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &PACKAGES.map(|name| (name, fixture.verified(name)))
        )
    );

    let output = fixture.baseline_output(VERSION, false);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(
        stdout,
        format!(
            "package baseline: release v1.2.3\npackage baseline: claude verified {}\npackage baseline: codex verified {}\npackage baseline: pi verified {}\n",
            fixture.released["claude"], fixture.released["codex"], fixture.released["pi"]
        )
    );
}

#[test]
fn a_package_edited_since_the_release_is_the_only_missing_one() {
    let fixture = Fixture::new();
    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &format!("{}# edited\n", descriptor("codex", "1.0.0")),
    );
    fixture.commit("edit codex without a version bump");
    let mut codex = missing("digest_mismatch");
    codex["version_not_bumped"] = json!(true);
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", codex),
                ("pi", fixture.verified("pi")),
            ]
        )
    );

    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &descriptor("codex", "1.0.1"),
    );
    fixture.commit("bump codex");
    let (actual, _) = fixture.baseline();
    assert_eq!(actual["packages"]["codex"], missing("digest_mismatch"));
}

#[test]
fn a_changed_compatibility_lock_is_missing() {
    let fixture = Fixture::new();
    fixture.write(
        "compat/pi/compatibility-lock.json",
        &lock("pi", "a later fixture run"),
    );
    fixture.commit("re-verify pi");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", fixture.verified("codex")),
                ("pi", missing("attestation_lock_mismatch")),
            ]
        )
    );
}

#[test]
fn a_changed_npm_lock_is_missing() {
    let fixture = Fixture::new();
    fixture.write(
        "compat/claude/npm/package-lock.json",
        "{\n  \"lockfileVersion\": 3,\n  \"packages\": {}\n}\n",
    );
    fixture.commit("refresh the claude npm lock");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", missing("changed_since_release")),
                ("codex", fixture.verified("codex")),
                ("pi", fixture.verified("pi")),
            ]
        )
    );
}

#[test]
fn uncommitted_and_untracked_compatibility_files_are_missing() {
    let fixture = Fixture::new();
    fixture.write("compat/pi/screens/idle.txt", "idle, recaptured\n");
    fixture.write("compat/codex/screens/new.txt", "new\n");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", missing("changed_since_release")),
                ("pi", missing("changed_since_release")),
            ]
        )
    );
}

#[test]
fn a_catalog_signed_by_a_key_outside_the_anchor_makes_every_package_missing() {
    let fixture = Fixture::new();
    let other = fixture.new_key("other");
    fixture.write_anchor(&other.public, 1, WINDOW_END);
    let (actual, stderr) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unverified"));
    assert!(
        stderr.contains("catalog is not signed by a known key"),
        "{stderr}"
    );
}

#[test]
fn an_anchor_window_that_excludes_now_makes_every_package_missing() {
    let fixture = Fixture::new();
    fixture.write_anchor(&fixture.key.public, 1, 2);
    let (actual, stderr) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unverified"));
    assert!(stderr.contains("signing key has expired"), "{stderr}");
}

#[test]
fn a_catalog_of_another_release_makes_every_package_missing() {
    let mut fixture = Fixture::new();
    let (actual, stderr) = fixture.baseline_for("1.2.4");
    assert_eq!(actual, all_missing("1.2.4", "release_mismatch"));
    assert!(
        stderr.contains("catalog is for release 1.2.3, not 1.2.4"),
        "{stderr}"
    );
    let (actual, _) = fixture.baseline_for("1.2.2");
    assert_eq!(actual, all_missing("1.2.2", "release_mismatch"));

    fixture.publish(&Release {
        sequence: SEQUENCE + 1,
        ..Release::default()
    });
    let (actual, stderr) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "release_mismatch"));
    assert!(
        stderr.contains(
            "catalog sequence 1099513724932 is not the sequence 1099513724931 of release 1.2.3"
        ),
        "{stderr}"
    );
}

#[test]
fn a_tampered_or_absent_archive_is_missing() {
    let fixture = Fixture::new();
    let codex = fixture
        .release_dir()
        .join("pohunek-runtime-codex-1.2.3.tar.zst");
    let mut bytes = std::fs::read(&codex).expect("read archive");
    *bytes.last_mut().expect("non-empty archive") ^= 0x01;
    std::fs::write(&codex, bytes).expect("tamper archive");
    std::fs::remove_file(
        fixture
            .release_dir()
            .join("pohunek-runtime-pi-1.2.3.tar.zst"),
    )
    .expect("remove archive");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", missing("archive_mismatch")),
                ("pi", missing("archive_absent")),
            ]
        )
    );
}

#[test]
fn an_attestation_that_does_not_match_its_catalog_digest_is_missing() {
    let fixture = Fixture::new();
    let attestation = fixture
        .release_dir()
        .join(format!("attestation-claude-{GNU}.json"));
    let mut bytes = std::fs::read(&attestation).expect("read attestation");
    bytes.push(b'\n');
    std::fs::write(&attestation, bytes).expect("modify attestation");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", missing("attestation_digest_mismatch")),
                ("codex", fixture.verified("codex")),
                ("pi", fixture.verified("pi")),
            ]
        )
    );
}

#[test]
fn a_matrix_row_without_an_attestation_is_missing() {
    let mut fixture = Fixture::unreleased();
    fixture.publish(&Release {
        targets: &[GNU],
        ..Release::default()
    });
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "platforms_uncovered"));
}

#[test]
fn a_new_consumer_suite_version_makes_every_package_missing() {
    let fixture = Fixture::new();
    fixture.write("compat/matrix.json", &matrix(&PACKAGES, SUITE_VERSION + 1));
    fixture.commit("raise the consumer suite version");
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "attestation_suite_mismatch"));
}

#[test]
fn no_catalog_makes_every_package_missing() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.catalog()).expect("remove catalog");
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_absent"));
}

#[test]
fn a_package_absent_from_the_catalog_is_missing() {
    let fixture = Fixture::new();
    fixture.add_package("hermes");
    fixture.write(
        "compat/matrix.json",
        &matrix(&["claude", "codex", "hermes", "pi"], SUITE_VERSION),
    );
    fixture.commit("add the hermes package");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", fixture.verified("codex")),
                ("hermes", missing("not_in_catalog")),
                ("pi", fixture.verified("pi")),
            ]
        )
    );
}

#[test]
fn an_unreachable_release_commit_makes_every_package_missing() {
    let mut fixture = Fixture::unreleased();
    fixture.publish(&Release {
        commit: Some(FOREIGN_COMMIT),
        ..Release::default()
    });
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "release_commit_unreachable"));
}

#[test]
fn an_unreadable_checkout_fails_without_verdicts() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.repo().join("compat/pi/compatibility-lock.json"))
        .expect("remove lock");
    let output = fixture.baseline_output(VERSION, true);
    assert_eq!(output.status.code(), Some(XTASK_FAILURE), "a missing lock");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);

    let fixture = Fixture::new();
    let output = fixture.baseline_output("1.2", true);
    assert_eq!(
        output.status.code(),
        Some(XTASK_FAILURE),
        "a malformed version"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

/// Gives one attestation per package a field that contradicts the catalog:
/// another package id, another release commit, another target's binary set.
fn contradict_catalog(package: &str, target: &str, document: &mut Value) {
    match (package, target) {
        ("claude", GNU) => document["package_id"] = json!("pohunek.runtime.other"),
        ("codex", MUSL) => document["commit"] = json!(FOREIGN_COMMIT),
        ("pi", GNU) => document["binary_set_digest"] = json!(BINARY_SETS[1].1),
        _ => {}
    }
}

/// Gives one attestation per package another runtime, platform or target.
fn contradict_reference(package: &str, target: &str, document: &mut Value) {
    match (package, target) {
        ("claude", GNU) => document["platform"] = json!(MUSL),
        ("codex", GNU) => document["runtime"] = json!("pi"),
        ("pi", MUSL) => document["target"] = json!(GNU),
        _ => {}
    }
}

/// Gives claude's GNU attestation another package digest and codex's MUSL
/// attestation another schema.
fn contradict_package(package: &str, target: &str, document: &mut Value) {
    match (package, target) {
        ("claude", GNU) => {
            document["package_digest"] =
                json!("sha256:3333333333333333333333333333333333333333333333333333333333333333");
        }
        ("codex", MUSL) => document["schema"] = json!(2),
        _ => {}
    }
}

#[test]
fn an_attestation_naming_another_release_or_package_is_invalid() {
    let mut fixture = Fixture::unreleased();
    fixture.publish(&Release {
        edit: contradict_catalog,
        ..Release::default()
    });
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "attestation_invalid"));
}

#[test]
fn an_attestation_of_another_runtime_platform_or_target_is_invalid() {
    let mut fixture = Fixture::unreleased();
    fixture.publish(&Release {
        edit: contradict_reference,
        ..Release::default()
    });
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "attestation_invalid"));
}

#[test]
fn an_attestation_of_another_package_digest_or_schema_is_missing() {
    let mut fixture = Fixture::unreleased();
    fixture.publish(&Release {
        edit: contradict_package,
        ..Release::default()
    });
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", missing("attestation_package_mismatch")),
                ("codex", missing("attestation_invalid")),
                ("pi", fixture.verified("pi")),
            ]
        )
    );
}

#[test]
fn an_absent_linked_or_non_file_attestation_is_missing() {
    let fixture = Fixture::new();
    std::fs::remove_file(
        fixture
            .release_dir()
            .join(format!("attestation-claude-{GNU}.json")),
    )
    .expect("remove attestation");
    fixture.replace_with_symlink(&format!("attestation-codex-{GNU}.json"));
    fixture.replace_with_directory(&format!("attestation-pi-{MUSL}.json"));
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", missing("attestation_absent")),
                ("codex", missing("attestation_unreadable")),
                ("pi", missing("attestation_unreadable")),
            ]
        )
    );
}

#[test]
fn a_linked_or_non_file_archive_is_missing() {
    let fixture = Fixture::new();
    fixture.replace_with_symlink("pohunek-runtime-codex-1.2.3.tar.zst");
    fixture.replace_with_directory("pohunek-runtime-pi-1.2.3.tar.zst");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", missing("archive_unreadable")),
                ("pi", missing("archive_unreadable")),
            ]
        )
    );
}

#[test]
fn a_linked_or_non_file_catalog_makes_every_package_missing() {
    let fixture = Fixture::new();
    fixture.replace_with_symlink("runtime-catalog.json");
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unreadable"));

    std::fs::remove_file(fixture.catalog()).expect("remove link");
    std::fs::create_dir(fixture.catalog()).expect("create directory");
    let (actual, _) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unreadable"));
}

#[test]
fn an_expired_catalog_makes_every_package_missing() {
    let mut fixture = Fixture::unreleased();
    fixture.publish(&Release {
        expires_at: 1,
        signing: Signing::InProcess,
        ..Release::default()
    });
    let (actual, stderr) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unverified"));
    assert!(stderr.contains("catalog has expired"), "{stderr}");
}

#[test]
fn an_anchor_root_not_yet_valid_makes_every_package_missing() {
    let fixture = Fixture::new();
    fixture.write_anchor(&fixture.key.public, WINDOW_END - 1, WINDOW_END);
    let (actual, stderr) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unverified"));
    assert!(stderr.contains("signing key is not yet valid"), "{stderr}");
}

#[test]
fn an_anchor_that_revokes_the_signer_makes_every_package_missing() {
    let fixture = Fixture::new();
    fixture.write_anchor_revoking(&fixture.key.public, 1, WINDOW_END, &[&fixture.key.id]);
    let (actual, stderr) = fixture.baseline();
    assert_eq!(actual, all_missing(VERSION, "catalog_unverified"));
    assert!(stderr.contains("signing key is revoked"), "{stderr}");
}

#[test]
fn an_ignored_file_in_a_compatibility_directory_is_missing() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "*.log\n");
    fixture.commit("ignore logs");
    fixture.write("compat/pi/screens/capture.log", "debug output\n");
    fixture.write("compat/codex/npm/install.log", "npm\n");
    let (actual, _) = fixture.baseline();
    assert_eq!(
        actual,
        verdicts(
            VERSION,
            &[
                ("claude", fixture.verified("claude")),
                ("codex", missing("changed_since_release")),
                ("pi", missing("changed_since_release")),
            ]
        )
    );
}

/// A gate verdict for one package, as `package bump --json` prints it.
fn gate(verdict: &str, reason: Option<&str>) -> Value {
    json!({ "verdict": verdict, "reason": reason })
}

/// The bump report of the fixture release with the overall `verdict`.
fn gate_report(verdict: &str, packages: &[(&str, Value)]) -> Value {
    let mut report = verdicts(VERSION, packages);
    report["verdict"] = json!(verdict);
    report
}

#[test]
fn an_unchanged_checkout_passes_the_bump_gate() {
    let fixture = Fixture::new();
    let (status, actual) = fixture.bump();
    assert!(status.success(), "{status}");
    assert_eq!(
        actual,
        gate_report("pass", &PACKAGES.map(|name| (name, gate("pass", None))),)
    );
}

#[test]
fn a_changed_package_with_a_bumped_version_passes_the_bump_gate() {
    let fixture = Fixture::new();
    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &format!("{}# edited\n", descriptor("codex", "1.0.1")),
    );
    fixture.commit("edit codex and bump its version");
    let (status, actual) = fixture.bump();
    assert!(status.success(), "{status}");
    assert_eq!(
        actual,
        gate_report("pass", &PACKAGES.map(|name| (name, gate("pass", None))),)
    );
}

#[test]
fn a_changed_package_without_a_version_bump_fails_the_bump_gate() {
    let fixture = Fixture::new();
    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &format!("{}# edited\n", descriptor("codex", "1.0.0")),
    );
    fixture.commit("edit codex without a version bump");
    let (status, actual) = fixture.bump();
    assert_eq!(status.code(), Some(XTASK_FAILURE));
    assert_eq!(
        actual,
        gate_report(
            "fail",
            &[
                ("claude", gate("pass", None)),
                ("codex", gate("fail", Some("version_not_bumped"))),
                ("pi", gate("pass", None)),
            ],
        )
    );
}

#[test]
fn a_changed_package_below_the_released_version_fails_the_bump_gate() {
    let fixture = Fixture::new();
    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &format!("{}# edited\n", descriptor("codex", "0.9.9")),
    );
    fixture.commit("edit codex and lower its version");
    let (status, actual) = fixture.bump();
    assert_eq!(status.code(), Some(XTASK_FAILURE));
    assert_eq!(
        actual,
        gate_report(
            "fail",
            &[
                ("claude", gate("pass", None)),
                ("codex", gate("fail", Some("version_not_bumped"))),
                ("pi", gate("pass", None)),
            ],
        )
    );
}

#[test]
fn a_package_new_to_the_release_passes_the_bump_gate() {
    let fixture = Fixture::new();
    fixture.add_package("hermes");
    fixture.commit("add hermes");
    let (status, actual) = fixture.bump();
    assert!(status.success(), "{status}");
    assert_eq!(
        actual,
        gate_report(
            "pass",
            &[
                ("claude", gate("pass", None)),
                ("codex", gate("pass", None)),
                ("hermes", gate("pass", None)),
                ("pi", gate("pass", None)),
            ],
        )
    );
}

#[test]
fn an_absent_catalog_leaves_the_bump_gate_undecided() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.catalog()).expect("remove catalog");
    let (status, actual) = fixture.bump();
    assert!(status.success(), "{status}");
    assert_eq!(
        actual,
        gate_report(
            "undecided",
            &PACKAGES.map(|name| (name, gate("undecided", Some("catalog_absent")))),
        )
    );
}

#[test]
fn a_catalog_that_does_not_verify_fails_the_bump_gate() {
    let fixture = Fixture::new();
    let other = fixture.new_key("other");
    fixture.write_anchor(&other.public, 1, WINDOW_END);
    let (status, actual) = fixture.bump();
    assert_eq!(status.code(), Some(XTASK_FAILURE));
    assert_eq!(
        actual,
        gate_report(
            "fail",
            &PACKAGES.map(|name| (name, gate("fail", Some("catalog_unverified")))),
        )
    );
}

#[test]
fn a_catalog_of_another_release_fails_the_bump_gate() {
    let fixture = Fixture::new();
    let (status, actual) = fixture.bump_for("1.2.4");
    assert_eq!(status.code(), Some(XTASK_FAILURE));
    assert_eq!(
        actual["packages"]["codex"],
        gate("fail", Some("release_mismatch"))
    );
    assert_eq!(actual["verdict"], json!("fail"));
}

/// Build metadata does not raise semver precedence, so it is no bump.
#[test]
fn a_changed_package_with_only_build_metadata_added_fails_the_bump_gate() {
    let fixture = Fixture::new();
    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &format!("{}# edited\n", descriptor("codex", "1.0.0+build.1")),
    );
    fixture.commit("edit codex and add build metadata to its version");
    let (status, actual) = fixture.bump();
    assert_eq!(status.code(), Some(XTASK_FAILURE));
    assert_eq!(
        actual["packages"]["codex"],
        gate("fail", Some("version_not_bumped"))
    );
}

#[test]
fn a_changed_package_with_a_version_that_is_not_semver_fails_the_bump_gate() {
    let fixture = Fixture::new();
    fixture.write(
        "runtime-packages/codex/runtime.toml",
        &format!("{}# edited\n", descriptor("codex", "1.1")),
    );
    fixture.commit("edit codex and use a two-part version");
    let (status, actual) = fixture.bump();
    assert_eq!(status.code(), Some(XTASK_FAILURE));
    assert_eq!(
        actual["packages"]["codex"],
        gate("fail", Some("version_not_bumped"))
    );
}
