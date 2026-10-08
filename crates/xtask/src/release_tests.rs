//! Scenarios for `release assemble` and `release verify-inventory`, run
//! through the command boundary with throwaway keys and small synthetic
//! archives. The official package directories, the lock files and the
//! compatibility matrix are the repository's own; everything else is built
//! here from scratch.

// Rust guideline compliant 2026-10-08

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ed25519_dalek::SigningKey;
use package::KeyId;

use crate::attestation::Fault as AttestationFault;
use crate::catalog::{anchor_to, hex, RootSpec};
use crate::release::{Fault, InputClass, ReleaseError};
use crate::release_tree::{extract_archive, sha256_bytes};
use crate::XtaskError;

const VERSION: &str = "1.2.3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_COMMIT: &str = "fedcba9876543210fedcba9876543210fedcba98";
const COMMIT_TIME: u64 = 1_800_000_000;
const GNU: &str = "x86_64-unknown-linux-gnu";
const MUSL: &str = "x86_64-unknown-linux-musl";
const MAC: &str = "aarch64-apple-darwin";
const SIGNER_SEED: u8 = 21;
const STRANGER_SEED: u8 = 22;
const ROOT_WINDOW: (u64, u64) = (1_700_000_000, 2_000_000_000);
const BINARIES: [&str; 3] = ["pohunekd", "pohunek", "pohunek-sessiond"];
const MINIMUM_MACOS: &str = "14.0";

/// `sequence` of version 1.2.3: `1 << 40 | 2 << 20 | 3`, computed by hand.
const EXPECTED_SEQUENCE: u64 = 1_099_513_724_931;

/// `expires_at`: the commit time plus 365 days of 86 400 seconds.
const EXPECTED_EXPIRY: u64 = 1_831_536_000;

fn root() -> PathBuf {
    pohunek_test_support::workspace_root()
}

fn script(name: &str, arguments: &[&OsStr]) {
    let status = Command::new("sh")
        .arg(root().join("packaging").join(name))
        .args(arguments)
        .env("SOURCE_DATE_EPOCH", COMMIT_TIME.to_string())
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .status()
        .expect("run packaging script");
    assert!(status.success(), "packaging/{name} failed");
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, bytes).expect("write");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Names of the official runtimes, read from the package directories.
fn runtimes() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root().join("runtime-packages"))
        .expect("runtime-packages")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn archive_name(component: &str, target: &str) -> String {
    format!("pohunek-{component}-{VERSION}-{target}.tar.gz")
}

/// Options of one daemon archive build.
struct Daemon<'a> {
    target: &'a str,
    anchor: &'a [u8],
    /// Mixed into the binaries so builds with different tags differ.
    tag: &'a str,
}

fn binary_bytes(target: &str, name: &str, tag: &str) -> Vec<u8> {
    format!("{name} for {target} build {tag}").into_bytes()
}

/// One set of consistent producer artifacts in a temporary directory.
struct Fixture {
    dir: tempfile::TempDir,
    anchor: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let keys = dir.path().join("keys");
        fs::create_dir(&keys).expect("keys dir");
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o700)).expect("chmod");
        for (name, seed) in [("signer", SIGNER_SEED), ("stranger", STRANGER_SEED)] {
            let key = SigningKey::from_bytes(&[seed; 32]);
            write_file(
                &keys.join(format!("{name}.key")),
                hex(&[seed; 32]).as_bytes(),
                0o600,
            );
            write_file(
                &keys.join(format!("{name}.pub")),
                hex(&key.verifying_key().to_bytes()).as_bytes(),
                0o644,
            );
        }
        let anchor_path = dir.path().join("anchor.json");
        anchor_to(
            &[RootSpec::new(
                &keys.join("signer.pub"),
                ROOT_WINDOW.0,
                ROOT_WINDOW.1,
            )],
            &[],
            &anchor_path,
        )
        .expect("anchor");
        let anchor = fs::read(&anchor_path).expect("read anchor");
        let fixture = Self { dir, anchor };
        fs::create_dir(fixture.inputs()).expect("inputs");
        fixture.build_all(COMMIT);
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn inputs(&self) -> PathBuf {
        self.path("inputs")
    }

    fn input(&self, name: &str) -> PathBuf {
        self.inputs().join(name)
    }

    fn out(&self, name: &str) -> PathBuf {
        self.path(name)
    }

    fn build_all(&self, commit: &str) {
        for runtime in runtimes() {
            self.build_package(&runtime, &root().join("runtime-packages").join(&runtime));
        }
        for target in [GNU, MUSL, MAC] {
            self.write_bins(target, "ci");
            self.write_daemon(&Daemon {
                target,
                anchor: &self.anchor,
                tag: "ci",
            });
        }
        for (component, target) in [("cli", MAC), ("cli", GNU), ("cli", MUSL), ("relay", GNU)] {
            self.write_plain_archive(component, target);
        }
        for name in ["protocol", "sdk", "testkit"] {
            let tarball = format!("pohunek-ts-{name}-{VERSION}.tgz");
            write_file(
                &self.input(&tarball),
                format!("sdk {name}").as_bytes(),
                0o644,
            );
            self.write_checksum(&tarball);
        }
        for target in [GNU, MUSL] {
            for runtime in runtimes() {
                self.attest(&runtime, target, commit);
            }
        }
    }

    fn build_package(&self, runtime: &str, source: &Path) {
        crate::run([
            "package".to_owned(),
            "build".into(),
            source.display().to_string(),
            "-o".into(),
            self.input(&format!("pohunek-runtime-{runtime}-{VERSION}.tar.zst"))
                .display()
                .to_string(),
        ])
        .expect("package build");
    }

    fn write_bins(&self, target: &str, tag: &str) {
        for name in BINARIES {
            write_file(
                &self.path("bins").join(target).join(name),
                &binary_bytes(target, name, tag),
                0o755,
            );
        }
    }

    fn write_checksum(&self, name: &str) {
        let digest = sha256_bytes(&fs::read(self.input(name)).expect("read"));
        write_file(
            &self.input(&format!("{name}.sha256")),
            format!("{digest}  {name}\n").as_bytes(),
            0o644,
        );
    }

    /// Seals `tree` as `<name>` into the inputs with the real packaging
    /// scripts and writes its checksum file.
    fn seal(&self, parent: &Path, name: &str, component: &str, target: &str) {
        let (signing, minimum): (&str, Option<&str>) = if target == MAC {
            ("adhoc", Some(MINIMUM_MACOS))
        } else {
            ("none", None)
        };
        let tree = parent.join(name);
        let mut arguments: Vec<&OsStr> = vec![
            tree.as_os_str(),
            OsStr::new(component),
            OsStr::new(VERSION),
            OsStr::new(target),
            OsStr::new(signing),
        ];
        arguments.extend(minimum.map(OsStr::new));
        script("write-manifest", &arguments);
        script(
            "archive",
            &[
                parent.as_os_str(),
                OsStr::new(name),
                self.inputs().as_os_str(),
            ],
        );
    }

    fn write_daemon(&self, daemon: &Daemon<'_>) {
        let name = format!("pohunek-daemon-{VERSION}-{}", daemon.target);
        let parent = self.path("trees").join(format!("daemon-{}", daemon.target));
        let _ = fs::remove_dir_all(&parent);
        let top = parent.join(&name);
        for binary in BINARIES {
            write_file(
                &top.join(binary),
                &binary_bytes(daemon.target, binary, daemon.tag),
                0o755,
            );
        }
        write_file(
            &top.join("runtime-catalog-anchor.json"),
            daemon.anchor,
            0o644,
        );
        write_file(
            &top.join("packaging/verify-archive"),
            &fs::read(root().join("packaging/verify-archive")).expect("verify-archive"),
            0o755,
        );
        write_file(&top.join("README.md"), b"readme\n", 0o644);
        write_file(&top.join("docs/install.md"), b"install\n", 0o644);
        self.seal(&parent, &name, "daemon", daemon.target);
    }

    fn write_plain_archive(&self, component: &str, target: &str) {
        let name = format!("pohunek-{component}-{VERSION}-{target}");
        let parent = self.path("trees").join(format!("{component}-{target}"));
        let _ = fs::remove_dir_all(&parent);
        write_file(&parent.join(&name).join("pohunek"), b"binary", 0o755);
        write_file(&parent.join(&name).join("README.md"), b"readme\n", 0o644);
        self.seal(&parent, &name, component, target);
    }

    fn attest(&self, runtime: &str, target: &str, commit: &str) {
        let lock_path = root()
            .join("compat")
            .join(runtime)
            .join("compatibility-lock.json");
        let lock: serde_json::Value =
            serde_json::from_slice(&fs::read(&lock_path).expect("lock")).expect("lock json");
        let package = self.input(&format!("pohunek-runtime-{runtime}-{VERSION}.tar.zst"));
        let executables: serde_json::Map<String, serde_json::Value> = BINARIES
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    sha256_bytes(&binary_bytes(target, name, "ci")).into(),
                )
            })
            .collect();
        let matrix: serde_json::Value = serde_json::from_slice(
            &fs::read(root().join("compat").join("matrix.json")).expect("matrix"),
        )
        .expect("matrix json");
        let report = serde_json::json!({
            "schema": 1,
            "runtime": runtime,
            "suite_version": matrix["suite_version"],
            "package_digest": format!("sha256:{}", sha256_bytes(&fs::read(&package).expect("pkg"))),
            "upstream_version": lock["upstream"]["release"],
            "executables": executables,
        });
        let report_path = self.path(&format!("report-{runtime}-{target}.json"));
        fs::write(&report_path, report.to_string()).expect("report");
        crate::run([
            "compat".to_owned(),
            "attest".into(),
            "--report".into(),
            report_path.display().to_string(),
            "--bin-dir".into(),
            self.path("bins").join(target).display().to_string(),
            "--package".into(),
            package.display().to_string(),
            "--lock".into(),
            lock_path.display().to_string(),
            "--matrix".into(),
            root().join("compat/matrix.json").display().to_string(),
            "--commit".into(),
            commit.into(),
            "--target".into(),
            target.into(),
            "--output".into(),
            self.input(&format!("attestation-{runtime}-{target}.json"))
                .display()
                .to_string(),
        ])
        .expect("compat attest");
    }

    fn key_id(seed: u8) -> String {
        KeyId::derive(&SigningKey::from_bytes(&[seed; 32]).verifying_key())
            .as_str()
            .to_owned()
    }

    fn args(&self, output: &Path) -> Vec<String> {
        let path = |value: &Path| value.display().to_string();
        vec![
            "release".to_owned(),
            "assemble".into(),
            "--version".into(),
            VERSION.into(),
            "--commit".into(),
            COMMIT.into(),
            "--commit-time".into(),
            COMMIT_TIME.to_string(),
            "--policy".into(),
            path(&root().join("packaging/release-policy.json")),
            "--matrix".into(),
            path(&root().join("compat/matrix.json")),
            "--inputs".into(),
            path(&self.inputs()),
            "--anchor".into(),
            path(&self.path("anchor.json")),
            "--key-file".into(),
            path(&self.path("keys/signer.key")),
            "--key-id".into(),
            Self::key_id(SIGNER_SEED),
            "--output".into(),
            path(output),
        ]
    }

    fn assemble(&self, output: &Path) -> Result<(), XtaskError> {
        crate::run(self.args(output))
    }

    /// Runs the assembler with `flag` set to `value`.
    fn assemble_with(&self, output: &Path, flag: &str, value: &str) -> Result<(), XtaskError> {
        let mut args = self.args(output);
        let position = args.iter().position(|arg| arg == flag).expect("flag");
        args[position + 1] = value.to_owned();
        crate::run(args)
    }

    /// Requires `result` to be a refusal of `class` for `fault` that left no
    /// output and no staging directory behind.
    fn expect_refused(&self, result: Result<(), XtaskError>, class: InputClass, fault: Fault) {
        match result {
            Err(XtaskError::Release(ReleaseError {
                class: got_class,
                fault: got_fault,
                name,
            })) => assert_eq!((got_class, got_fault), (class, fault), "refused {name:?}"),
            other => panic!("expected a {class:?}/{fault:?} refusal, got {other:?}"),
        }
        self.assert_nothing_left();
    }

    fn assert_nothing_left(&self) {
        assert!(
            !self.out("bundle").exists(),
            "a refused release wrote its output"
        );
        let strays: Vec<String> = fs::read_dir(self.dir.path())
            .expect("list")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".pohunek-"))
            .collect();
        assert!(strays.is_empty(), "staging left behind: {strays:?}");
    }

    fn replace_input(&self, name: &str, bytes: &[u8]) {
        write_file(&self.input(name), bytes, 0o644);
        if self.input(&format!("{name}.sha256")).exists() {
            self.write_checksum(name);
        }
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn extract(archive: &Path, scratch: &Path) -> PathBuf {
    let name = archive
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    let stem = name.strip_suffix(".tar.gz").expect("tar.gz").to_owned();
    fs::create_dir_all(scratch).expect("mkdir");
    extract_archive(archive, &name, &stem, scratch).expect("extract")
}

/// A gzip tar archive whose members are given as raw header names, so a
/// hostile name can be written.
fn hostile_archive(top: &str, members: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut entries: Vec<(String, tar::EntryType, Vec<u8>)> =
        vec![(format!("{top}/"), tar::EntryType::Directory, Vec::new())];
    entries.extend(
        members
            .iter()
            .map(|(name, kind, data)| ((*name).to_owned(), *kind, data.to_vec())),
    );
    for (name, kind, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(if kind == tar::EntryType::Regular {
            data.len() as u64
        } else {
            0
        });
        header.set_mode(0o644);
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        if kind == tar::EntryType::Symlink {
            header.set_link_name("/etc/passwd").expect("link");
        }
        header.set_cksum();
        builder.append(&header, data.as_slice()).expect("append");
    }
    let tarball = builder.into_inner().expect("tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tarball).expect("gzip");
    encoder.finish().expect("finish")
}

#[test]
fn a_complete_set_assembles_a_verifiable_reproducible_bundle() {
    let fixture = Fixture::new();
    let first = fixture.out("bundle");
    fixture.assemble(&first).expect("assemble");
    let second = fixture.out("bundle-again");
    fixture.assemble(&second).expect("assemble again");

    let names = names_in(&first);
    assert_eq!(names, names_in(&second));
    let mut expected: Vec<String> = Vec::new();
    for archive in [
        format!("pohunek-cli-{VERSION}-{MAC}.tar.gz"),
        format!("pohunek-cli-{VERSION}-{GNU}.tar.gz"),
        format!("pohunek-cli-{VERSION}-{MUSL}.tar.gz"),
        format!("pohunek-daemon-{VERSION}-{MAC}.tar.gz"),
        format!("pohunek-daemon-{VERSION}-{GNU}.tar.gz"),
        format!("pohunek-daemon-{VERSION}-{MUSL}.tar.gz"),
        format!("pohunek-relay-{VERSION}-{GNU}.tar.gz"),
        format!("pohunek-ts-protocol-{VERSION}.tgz"),
        format!("pohunek-ts-sdk-{VERSION}.tgz"),
        format!("pohunek-ts-testkit-{VERSION}.tgz"),
    ] {
        expected.push(format!("{archive}.sha256"));
        expected.push(archive);
    }
    for runtime in runtimes() {
        expected.push(format!("pohunek-runtime-{runtime}-{VERSION}.tar.zst"));
        for target in [GNU, MUSL] {
            expected.push(format!("attestation-{runtime}-{target}.json"));
        }
    }
    expected.push("runtime-catalog.json".to_owned());
    expected.push("release-inventory.sha256".to_owned());
    expected.sort();
    assert_eq!(names, expected);
    for name in &names {
        assert_eq!(
            fs::read(first.join(name)).expect("first"),
            fs::read(second.join(name)).expect("second"),
            "{name} differs between two assemblies of the same inputs"
        );
    }

    // The inventory covers every file but itself, in `sha256sum -c` format.
    let inventory = fs::read_to_string(first.join("release-inventory.sha256")).expect("inventory");
    let listed: Vec<&str> = inventory
        .lines()
        .map(|line| line.split_once("  ").expect("two spaces").1)
        .collect();
    let mut others: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| *name != "release-inventory.sha256")
        .collect();
    others.sort_unstable();
    assert_eq!(listed, others);
    let checked = Command::new("sha256sum")
        .arg("-c")
        .arg("release-inventory.sha256")
        .current_dir(&first)
        .output()
        .expect("sha256sum");
    assert!(
        checked.status.success(),
        "sha256sum -c rejected the inventory"
    );

    crate::run([
        "release".to_owned(),
        "verify-inventory".into(),
        "--dir".into(),
        first.display().to_string(),
        "--now".into(),
        COMMIT_TIME.to_string(),
    ])
    .expect("verify-inventory");

    let verified = crate::catalog::verify(
        &first.join("runtime-catalog.json"),
        &fixture.path("anchor.json"),
        None,
        COMMIT_TIME,
    )
    .expect("catalog verifies against the anchor");
    assert_eq!(verified.sequence, EXPECTED_SEQUENCE);
    assert_eq!(verified.expires_at, EXPECTED_EXPIRY);
    assert_eq!(verified.entries.len(), runtimes().len());
    for entry in &verified.entries {
        assert!(
            entry.contains(&format!("[{GNU}, {MUSL}]")) && entry.ends_with(&format!(" ={VERSION}")),
            "unexpected entry {entry}"
        );
    }
}

#[test]
fn a_daemon_bundle_holds_exactly_the_packages_its_target_attests() {
    let fixture = Fixture::new();
    let bundle = fixture.out("bundle");
    fixture.assemble(&bundle).expect("assemble");
    let scratch = fixture.path("unpacked");
    let catalog = fs::read(bundle.join("runtime-catalog.json")).expect("catalog");

    for target in [GNU, MUSL] {
        let top = extract(
            &bundle.join(archive_name("daemon", target)),
            &scratch.join(target),
        );
        let runtime = top.join("runtime");
        assert_eq!(
            names_in(&runtime),
            ["attestations", "packages", "runtime-catalog.json"]
        );
        assert_eq!(
            fs::read(runtime.join("runtime-catalog.json")).expect("catalog"),
            catalog
        );
        let packages: Vec<String> = runtimes()
            .iter()
            .map(|name| format!("{name}.tar.zst"))
            .collect();
        assert_eq!(names_in(&runtime.join("packages")), packages);
        let attestations: Vec<String> = runtimes()
            .iter()
            .map(|name| format!("{name}-{target}.json"))
            .collect();
        assert_eq!(names_in(&runtime.join("attestations")), attestations);
        for name in runtimes() {
            assert_eq!(
                fs::read(runtime.join("packages").join(format!("{name}.tar.zst"))).expect("pkg"),
                fs::read(bundle.join(format!("pohunek-runtime-{name}-{VERSION}.tar.zst")))
                    .expect("top-level package"),
            );
        }
    }

    // The macOS archive carries the signed catalog and no package, and keeps
    // the producer's signing state and deployment target.
    let mac = extract(
        &bundle.join(archive_name("daemon", MAC)),
        &scratch.join(MAC),
    );
    assert_eq!(names_in(&mac.join("runtime")), ["runtime-catalog.json"]);
    let manifest = fs::read_to_string(mac.join("MANIFEST")).expect("manifest");
    assert!(manifest.contains("\nsigning adhoc\n"), "{manifest}");
    assert!(manifest.contains(&format!("\nminimum-macos {MINIMUM_MACOS}\n")));
    assert!(manifest.contains("runtime/runtime-catalog.json"));
    assert!(!manifest.contains("runtime/packages"));

    // The installer's own archive verifier accepts the Linux tree on a host
    // of its target.
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        let top = scratch
            .join(GNU)
            .join(format!("pohunek-daemon-{VERSION}-{GNU}"));
        let status = Command::new("sh")
            .arg(root().join("packaging/verify-archive"))
            .arg(&top)
            .args(["daemon", "pohunekd", "pohunek", "pohunek-sessiond"])
            .status()
            .expect("verify-archive");
        assert!(
            status.success(),
            "packaging/verify-archive refused the assembled tree"
        );
    }
}

#[test]
fn a_missing_attestation_row_is_refused() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.input(&format!("attestation-{}-{MUSL}.json", runtimes()[0])))
        .expect("rm");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Attestation,
        Fault::Missing,
    );
}

#[test]
fn an_attestation_row_outside_the_matrix_is_refused() {
    let fixture = Fixture::new();
    let name = format!("attestation-{}-{MAC}.json", runtimes()[0]);
    fs::write(fixture.input(&name), "{}").expect("extra row");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Unexpected,
        Fault::Unexpected,
    );
}

#[test]
fn a_second_package_for_one_runtime_is_refused_as_a_duplicate() {
    let fixture = Fixture::new();
    let runtime = &runtimes()[0];
    fs::copy(
        fixture.input(&format!("pohunek-runtime-{runtime}-{VERSION}.tar.zst")),
        fixture.input(&format!("pohunek-runtime-{runtime}-9.9.9.tar.zst")),
    )
    .expect("copy");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::PackageArchive,
        Fault::Duplicate,
    );
}

#[test]
fn a_package_archive_swapped_after_attestation_is_refused() {
    let fixture = Fixture::new();
    let runtime = &runtimes()[0];
    let source = fixture.path("swapped-source");
    fs::create_dir(&source).expect("mkdir");
    for entry in fs::read_dir(root().join("runtime-packages").join(runtime)).expect("list") {
        let entry = entry.expect("entry");
        fs::copy(entry.path(), source.join(entry.file_name())).expect("copy");
    }
    let mut descriptor = fs::read(source.join("runtime.toml")).expect("descriptor");
    descriptor.extend_from_slice(b"\n# swapped after the attestation\n");
    fs::write(source.join("runtime.toml"), descriptor).expect("write");
    fixture.build_package(runtime, &source);
    let result = fixture.assemble(&fixture.out("bundle"));
    match result {
        Err(XtaskError::Attestation(error)) => {
            assert_eq!(
                (error.field.as_str(), error.fault),
                ("package_digest", AttestationFault::Mismatch)
            );
        }
        other => panic!("expected a package_digest refusal, got {other:?}"),
    }
    fixture.assert_nothing_left();
}

#[test]
fn a_daemon_binary_changed_after_attestation_is_refused() {
    let fixture = Fixture::new();
    fixture.write_daemon(&Daemon {
        target: GNU,
        anchor: &fixture.anchor,
        tag: "rebuilt-after-attestation",
    });
    match fixture.assemble(&fixture.out("bundle")) {
        Err(XtaskError::Attestation(error)) => {
            assert_eq!(
                (error.field.as_str(), error.fault),
                ("binary_set_digest", AttestationFault::Mismatch)
            );
        }
        other => panic!("expected a binary_set_digest refusal, got {other:?}"),
    }
    fixture.assert_nothing_left();
}

#[test]
fn an_attestation_from_another_commit_is_refused() {
    let fixture = Fixture::new();
    let runtime = &runtimes()[0];
    fixture.attest(runtime, GNU, OTHER_COMMIT);
    match fixture.assemble(&fixture.out("bundle")) {
        Err(XtaskError::Attestation(error)) => {
            assert_eq!(
                (error.field.as_str(), error.fault),
                ("commit", AttestationFault::Mismatch)
            );
        }
        other => panic!("expected a commit refusal, got {other:?}"),
    }
    fixture.assert_nothing_left();
}

#[test]
fn an_attestation_for_another_target_is_refused() {
    let fixture = Fixture::new();
    let runtime = &runtimes()[0];
    fs::copy(
        fixture.input(&format!("attestation-{runtime}-{MUSL}.json")),
        fixture.input(&format!("attestation-{runtime}-{GNU}.json")),
    )
    .expect("copy");
    match fixture.assemble(&fixture.out("bundle")) {
        Err(XtaskError::Attestation(error)) => {
            assert_eq!(
                (error.field.as_str(), error.fault),
                ("target", AttestationFault::Mismatch)
            );
        }
        other => panic!("expected a target refusal, got {other:?}"),
    }
    fixture.assert_nothing_left();
}

#[test]
fn a_wrong_checksum_file_is_refused() {
    let fixture = Fixture::new();
    let name = format!("pohunek-ts-sdk-{VERSION}.tgz");
    let wrong = format!("{}  {name}\n", "0".repeat(64));
    fs::write(fixture.input(&format!("{name}.sha256")), wrong).expect("write");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Checksum,
        Fault::ChecksumMismatch,
    );
}

#[test]
fn an_archive_missing_from_the_policy_is_refused() {
    let fixture = Fixture::new();
    let name = archive_name("cli", MAC);
    fs::remove_file(fixture.input(&name)).expect("rm");
    fs::remove_file(fixture.input(&format!("{name}.sha256"))).expect("rm");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Archive,
        Fault::Missing,
    );
}

#[test]
fn an_unexpected_file_is_refused() {
    let fixture = Fixture::new();
    fs::write(fixture.input("notes.txt"), "extra").expect("write");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Unexpected,
        Fault::Unexpected,
    );
}

#[test]
fn a_daemon_archive_with_another_anchor_is_refused() {
    let fixture = Fixture::new();
    let mut other = fixture.anchor.clone();
    other.push(b'\n');
    fixture.write_daemon(&Daemon {
        target: MUSL,
        anchor: &other,
        tag: "ci",
    });
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Anchor,
        Fault::Mismatch,
    );
}

#[test]
fn a_catalog_signed_by_a_key_the_anchor_does_not_list_is_refused() {
    let fixture = Fixture::new();
    let output = fixture.out("bundle");
    let mut args = fixture.args(&output);
    for (flag, value) in [
        (
            "--key-file",
            fixture.path("keys/stranger.key").display().to_string(),
        ),
        ("--key-id", Fixture::key_id(STRANGER_SEED)),
    ] {
        let position = args.iter().position(|arg| arg == flag).expect("flag");
        args[position + 1] = value;
    }
    assert!(
        matches!(crate::run(args), Err(XtaskError::Catalog(_))),
        "a catalog from an unlisted signer was accepted"
    );
    fixture.assert_nothing_left();
}

#[test]
fn a_version_component_of_two_to_the_twenty_is_refused() {
    let fixture = Fixture::new();
    let result = fixture.assemble_with(&fixture.out("bundle"), "--version", "1.1048576.0");
    fixture.expect_refused(result, InputClass::Version, Fault::OutOfRange);
    let result = fixture.assemble_with(&fixture.out("bundle"), "--version", "1.2");
    fixture.expect_refused(result, InputClass::Version, Fault::Malformed);
}

#[test]
fn a_traversal_member_in_an_input_archive_is_refused() {
    let fixture = Fixture::new();
    let name = archive_name("daemon", GNU);
    let top = format!("pohunek-daemon-{VERSION}-{GNU}");
    let hostile = hostile_archive(
        &top,
        &[("../../escape", tar::EntryType::Regular, b"escaped")],
    );
    fixture.replace_input(&name, &hostile);
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Archive,
        Fault::UnsafePath,
    );
    assert!(!fixture.dir.path().join("escape").exists());
}

#[test]
fn a_link_member_in_an_input_archive_is_refused() {
    let fixture = Fixture::new();
    let name = archive_name("daemon", MUSL);
    let top = format!("pohunek-daemon-{VERSION}-{MUSL}");
    let hostile = hostile_archive(
        &top,
        &[(&format!("{top}/pohunekd"), tar::EntryType::Symlink, b"")],
    );
    fixture.replace_input(&name, &hostile);
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Archive,
        Fault::UnsupportedMember,
    );
}

#[test]
fn an_output_directory_that_is_taken_is_refused_and_left_alone() {
    let fixture = Fixture::new();
    let output = fixture.out("bundle");
    fs::create_dir(&output).expect("mkdir");
    fs::write(output.join("keep"), "mine").expect("write");
    match fixture.assemble(&output) {
        Err(XtaskError::Release(error)) => {
            assert_eq!(
                (error.class, error.fault),
                (InputClass::Output, Fault::AlreadyExists)
            );
        }
        other => panic!("expected an output refusal, got {other:?}"),
    }
    assert_eq!(fs::read(output.join("keep")).expect("kept"), b"mine");
}

fn assembled() -> (Fixture, PathBuf) {
    let fixture = Fixture::new();
    let bundle = fixture.out("bundle");
    fixture.assemble(&bundle).expect("assemble");
    (fixture, bundle)
}

fn verify_inventory(bundle: &Path) -> Result<(), XtaskError> {
    crate::run([
        "release".to_owned(),
        "verify-inventory".into(),
        "--dir".into(),
        bundle.display().to_string(),
        "--now".into(),
        COMMIT_TIME.to_string(),
    ])
}

fn expect_inventory_refusal(result: Result<(), XtaskError>, class: InputClass, fault: Fault) {
    match result {
        Err(XtaskError::Release(error)) => {
            assert_eq!(
                (error.class, error.fault),
                (class, fault),
                "refused {:?}",
                error.name
            );
        }
        other => panic!("expected a {class:?}/{fault:?} refusal, got {other:?}"),
    }
}

#[test]
fn the_inventory_check_refuses_a_changed_a_missing_and_an_extra_file() {
    let (_fixture, bundle) = assembled();
    let sdk = bundle.join(format!("pohunek-ts-sdk-{VERSION}.tgz"));
    let original = fs::read(&sdk).expect("read");
    fs::write(&sdk, b"changed").expect("write");
    expect_inventory_refusal(
        verify_inventory(&bundle),
        InputClass::Inventory,
        Fault::ChecksumMismatch,
    );
    fs::remove_file(&sdk).expect("rm");
    expect_inventory_refusal(
        verify_inventory(&bundle),
        InputClass::Inventory,
        Fault::Missing,
    );
    fs::write(&sdk, original).expect("restore");
    fs::write(bundle.join("stray.txt"), "x").expect("write");
    expect_inventory_refusal(
        verify_inventory(&bundle),
        InputClass::Inventory,
        Fault::Unexpected,
    );
    fs::remove_file(bundle.join("stray.txt")).expect("rm");
    fs::create_dir(bundle.join("subdir")).expect("mkdir");
    expect_inventory_refusal(
        verify_inventory(&bundle),
        InputClass::Inventory,
        Fault::NotRegularFile,
    );
}

#[test]
fn the_inventory_check_refuses_a_catalog_the_archives_do_not_carry() {
    let (_fixture, bundle) = assembled();
    // A re-sealed inventory does not make a replaced catalog acceptable.
    let catalog = bundle.join("runtime-catalog.json");
    let mut bytes = fs::read(&catalog).expect("read");
    bytes.push(b'\n');
    fs::write(&catalog, bytes).expect("write");
    crate::release_inventory::write_inventory(&bundle).expect("reseal");
    assert!(
        verify_inventory(&bundle).is_err(),
        "a catalog the archives do not carry was accepted"
    );
}

#[test]
fn the_inventory_check_refuses_a_package_that_differs_from_the_catalog() {
    let (_fixture, bundle) = assembled();
    let runtime = &runtimes()[0];
    let package = bundle.join(format!("pohunek-runtime-{runtime}-{VERSION}.tar.zst"));
    let other = &runtimes()[1];
    fs::copy(
        bundle.join(format!("pohunek-runtime-{other}-{VERSION}.tar.zst")),
        &package,
    )
    .expect("swap");
    crate::release_inventory::write_inventory(&bundle).expect("reseal");
    expect_inventory_refusal(
        verify_inventory(&bundle),
        InputClass::PackageArchive,
        Fault::Mismatch,
    );
}

#[test]
fn a_symlinked_input_is_refused_before_it_is_read() {
    let fixture = Fixture::new();
    let name = format!("pohunek-ts-sdk-{VERSION}.tgz");
    let real = fixture.path("real.tgz");
    fs::rename(fixture.input(&name), &real).expect("move");
    symlink(&real, fixture.input(&name)).expect("link");
    fixture.expect_refused(
        fixture.assemble(&fixture.out("bundle")),
        InputClass::Unexpected,
        Fault::NotRegularFile,
    );
}

#[test]
fn the_checked_in_policy_lists_every_archive_the_release_workflow_builds() {
    let policy = crate::release_policy::load_policy(&root().join("packaging/release-policy.json"))
        .expect("policy");
    let mut slots: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for slot in &policy.archives {
        slots
            .entry(slot.archive_name(VERSION))
            .or_default()
            .push(slot.target.clone());
    }
    assert_eq!(slots.len(), 7);
    assert_eq!(policy.daemon_targets(), [MAC, GNU, MUSL]);
    assert_eq!(policy.catalog_validity_days, 365);
    assert_eq!(policy.sdk_packages, ["protocol", "sdk", "testkit"]);
}
