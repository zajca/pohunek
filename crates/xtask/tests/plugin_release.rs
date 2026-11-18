//! Scenarios for `xtask plugin build` and `xtask plugin prove`, driven through
//! the real `xtask` binary. `prove` runs against the workspace `pohunek`,
//! `pohunekd` and `pohunek-sessiond` binaries, so each proof starts a real
//! isolated daemon and installs through the real CLI.
//!
//! The expected results are stated outside the code under test: the digest is
//! recomputed with SHA-256 over the file bytes and with the package crate's
//! archive reader, the evidence shape and exit statuses are restated as
//! literals, and the isolation is observed from the outside (the temporary
//! directory the proof ran in, a decoy control socket at the caller's runtime
//! path, a marker program the proof must never launch).
//!
//! - building a package directory twice gives identical archives, an existing
//!   archive is never replaced, and an unusable output or input writes nothing;
//! - a proof of the fixture passes, removes its prefix and never touches the
//!   caller's runtime directory or launches the runtime program;
//! - a runtime id an index never authorizes fails at `install`, a differing
//!   digest, id or version at `digest` for either kind, a runtime-shaped or
//!   `[context]`-less descriptor at `digest`, a damaged assistant package at
//!   `inspect`, a missing or exiting daemon at `host`;
//! - a `--no-enable` install fails at `inspect` for being disabled, and a
//!   proof alongside another installed version of the same package fails at
//!   `install` on the default path and at `inspect` under `--no-enable`,
//!   where the digest check refuses the other, selected version;
//! - a `--pre-install` archive with a foreign package id is refused at
//!   `install`;
//! - a proof that cannot run writes no evidence and does not replace an
//!   existing one.

// Rust guideline compliant 2026-11-06

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Output;

use package::{host_platform, read_archive, Limits};
use pohunek_test_support::env::TestEnv;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// Package id, runtime id and version of the fixture package.
const PACKAGE_ID: &str = "pohunek.runtime.kitfixture";
const RUNTIME_ID: &str = "kitfixture";
const VERSION: &str = "0.1.0";

/// File name `plugin build` gives the fixture archive.
const ARCHIVE_NAME: &str = "pohunek.runtime.kitfixture-0.1.0.tar.zst";

/// Prefix of the directory a proof creates in `TMPDIR`: the kit's own
/// root prefix, so the leftover scan sees the roots the kit engine
/// creates for `xtask plugin prove`.
const PROOF_ROOT_PREFIX: &str = plugin_kit::isolated::ROOT_PREFIX;

/// Core range every core version satisfies.
const ANY_CORE: &str = ">=0.0.0";

/// Digest no archive has.
const ABSENT_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

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

fn fixture_dir() -> PathBuf {
    pohunek_test_support::manifest_dir().join("src/plugin/fixtures/package")
}

/// The directory that holds the workspace binaries `prove` installs.
fn release_dir() -> PathBuf {
    let dir = pohunek_test_support::bin_exe("xtask")
        .parent()
        .expect("the xtask binary has a directory")
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

/// A hermetic environment with the checkout marker `xtask` requires.
struct Plugin {
    env: TestEnv,
}

impl Plugin {
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

    fn xtask(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .current_dir(self.env.cwd())
            .args(args)
            .output()
            .expect("run xtask")
    }

    /// A copy of the fixture package whose `runtime.toml` has `from`
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
        dir
    }

    /// Builds `package` into a new directory and returns the archive path and
    /// its digest.
    fn build(&self, package: &Path, out: &str) -> (PathBuf, String) {
        self.build_as(package, out, ARCHIVE_NAME)
    }

    /// Builds `package` into a new directory and returns the archive of
    /// `archive_name` and its digest.
    fn build_as(&self, package: &Path, out: &str, archive_name: &str) -> (PathBuf, String) {
        let out = self.dir(out);
        let output = self.xtask(&[
            "plugin",
            "build",
            "--dir",
            s(package),
            "--output-dir",
            s(&out),
        ]);
        assert!(
            output.status.success(),
            "plugin build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let fields: Vec<&str> = stdout.trim_end().split(' ').collect();
        assert_eq!(fields.len(), 7, "one line of seven fields: {stdout:?}");
        (out.join(archive_name), fields[5].to_owned())
    }

    /// A context package directory: a descriptor with no runtime plus the
    /// assistant prompt contract, so even the assistant step can run. The
    /// `without_prompt` variant omits `assistant/system.md`, so the
    /// assistant readback has a damaged package to refuse.
    fn context_package(&self, name: &str, package_id: &str, version: &str) -> PathBuf {
        self.context_package_as(name, package_id, version, true)
    }

    fn context_package_as(
        &self,
        name: &str,
        package_id: &str,
        version: &str,
        with_prompt: bool,
    ) -> PathBuf {
        self.context_package_with_descriptor(name, package_id, version, with_prompt, None)
    }

    /// Like [`Self::context_package_as`], with the descriptor body replaced
    /// by `descriptor` when it is `Some`.
    fn context_package_with_descriptor(
        &self,
        name: &str,
        package_id: &str,
        version: &str,
        with_prompt: bool,
        descriptor: Option<&str>,
    ) -> PathBuf {
        let dir = self.dir(name);
        let descriptor = descriptor.map_or_else(
            || {
                format!(
                "schema = 1\nid = \"{package_id}\"\nversion = \"{version}\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n"
            )
            },
            str::to_owned,
        );
        fs::write(dir.join("runtime.toml"), descriptor).expect("write the descriptor");
        let bundle = dir.join("bundle");
        fs::create_dir_all(bundle.join("assistant")).expect("bundle directories");
        fs::write(bundle.join("index.md"), "# Index\n").expect("write the index");
        if with_prompt {
            fs::write(
                bundle.join("assistant/system.md"),
                "---\ntype: PromptTemplate\nid: assistant/system\ntitle: Contract\ndescription: Fixture contract.\nsource_kind: manual\nintents: [help]\n---\n\n## Safety\n\n- Never print secrets.\n\n## Navigation\n\n1. Start at the index.\n",
            )
            .expect("write the prompt contract");
        }
        dir
    }

    /// A context entry file for `archive` with the given identity.
    fn context_entry(&self, name: &str, package_id: &str, digest: &str, version: &str) -> PathBuf {
        let text = format!(
            "package_id = \"{package_id}\"\n\
             kind = \"context\"\n\
             version = \"{version}\"\n\
             digest = \"{digest}\"\n\
             url = \"https://github.com/pohunekd/plugin-kitfixture/releases/download/v{version}/{package_id}-{version}.tar.zst\"\n\
             platforms = [\"{}\"]\n\
             core = \">=0.42.0\"\n\n\
             [source]\n\
             repo = \"pohunekd/plugin-kitfixture\"\n\
             repo_id = 1001\n\
             tag = \"v{version}\"\n\
             commit = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\
             built_with = \"v0.42.0\"\n",
            host_platform().expect("this platform has an index platform name")
        );
        let path = self.path(name);
        fs::write(&path, text).expect("write the entry");
        path
    }

    /// An entry file for `archive` with the given identity.
    fn entry(&self, name: &str, runtime_id: &str, digest: &str) -> PathBuf {
        let text = format!(
            "package_id = \"{PACKAGE_ID}\"\n\
             runtime_id = \"{runtime_id}\"\n\
             version = \"{VERSION}\"\n\
             digest = \"{digest}\"\n\
             url = \"https://github.com/pohunekd/plugin-kitfixture/releases/download/v{VERSION}/{ARCHIVE_NAME}\"\n\
             platforms = [\"{}\"]\n\
             core = \"{ANY_CORE}\"\n\n\
             [source]\n\
             repo = \"pohunekd/plugin-kitfixture\"\n\
             repo_id = 1001\n\
             tag = \"v{VERSION}\"\n\
             commit = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\
             built_with = \"v0.40.0\"\n",
            host_platform().expect("this platform has an index platform name")
        );
        let path = self.path(name);
        fs::write(&path, text).expect("write the entry");
        path
    }

    fn prove(&self, archive: &Path, entry: &Path, release: &Path, evidence: &Path) -> Output {
        self.xtask(&[
            "plugin",
            "prove",
            "--archive",
            s(archive),
            "--entry",
            s(entry),
            "--release-dir",
            s(release),
            "--evidence",
            s(evidence),
        ])
    }

    fn prove_no_enable(
        &self,
        archive: &Path,
        entry: &Path,
        release: &Path,
        evidence: &Path,
    ) -> Output {
        self.xtask(&[
            "plugin",
            "prove",
            "--archive",
            s(archive),
            "--entry",
            s(entry),
            "--release-dir",
            s(release),
            "--evidence",
            s(evidence),
            "--no-enable",
        ])
    }

    /// A proof that installs `pre_install` alongside the archive first, so
    /// the proved version runs next to another installed version.
    fn prove_with_pre_install(
        &self,
        archive: &Path,
        entry: &Path,
        release: &Path,
        evidence: &Path,
        pre_install: &Path,
        no_enable: bool,
    ) -> Output {
        let mut args = vec![
            "plugin".to_owned(),
            "prove".to_owned(),
            "--archive".to_owned(),
            s(archive).to_owned(),
            "--entry".to_owned(),
            s(entry).to_owned(),
            "--release-dir".to_owned(),
            s(release).to_owned(),
            "--evidence".to_owned(),
            s(evidence).to_owned(),
            "--pre-install".to_owned(),
            s(pre_install).to_owned(),
        ];
        if no_enable {
            args.push("--no-enable".to_owned());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.xtask(&refs)
    }

    /// Proof roots left in the kit's `TMPDIR`: only the directory this test
    /// gave the kit. The kit falls back to its short system parent when
    /// the temp root is too long for a socket path; that fallback case is
    /// covered in `crates/plugin-kit/tests/kit.rs` instead, so parallel
    /// tests never see each other's roots here.
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
fn build_is_deterministic_and_never_replaces_an_archive() {
    let plugin = Plugin::new();
    let first = plugin.dir("first");
    let second = plugin.dir("second");
    let run = |out: &Path| {
        plugin.xtask(&[
            "plugin",
            "build",
            "--dir",
            s(&fixture_dir()),
            "--output-dir",
            s(out),
        ])
    };

    let output = run(&first);
    assert!(output.status.success(), "{}", stderr_of(&output));
    // The second build reads a copy whose files are a year older: an archive
    // that records modification times would differ.
    let copy = plugin.variant("copy", "version = \"0.1.0\"", "version = \"0.1.0\"");
    let old = std::time::SystemTime::now() - std::time::Duration::from_hours(365 * 24);
    for file in ["runtime.toml", "detect.toml"] {
        fs::File::options()
            .write(true)
            .open(copy.join(file))
            .and_then(|handle| handle.set_modified(old))
            .expect("age the copy");
    }
    let output_second = plugin.xtask(&[
        "plugin",
        "build",
        "--dir",
        s(&copy),
        "--output-dir",
        s(&second),
    ]);
    assert!(
        output_second.status.success(),
        "{}",
        stderr_of(&output_second)
    );
    let archive = first.join(ARCHIVE_NAME);
    let bytes = fs::read(&archive).expect("the archive exists");
    assert_eq!(
        bytes,
        fs::read(second.join(ARCHIVE_NAME)).expect("second archive")
    );
    assert_eq!(
        fs::metadata(&archive)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o644
    );

    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let digest = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
    assert_eq!(
        stdout,
        format!(
            "plugin build ok: {PACKAGE_ID} {VERSION} {digest} {}\n",
            archive.display()
        )
    );
    let verified = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    assert_eq!(verified.digest().as_str(), digest);

    let third = run(&first);
    assert!(!third.status.success());
    assert_eq!(fs::read(&archive).expect("archive after a refusal"), bytes);
}

#[test]
fn build_archives_a_context_package_that_serves_no_runtime() {
    let plugin = Plugin::new();
    let package = plugin.dir("context-package");
    fs::write(
        package.join("runtime.toml"),
        "schema = 1\nid = \"pohunek.context.fixture\"\nversion = \"0.2.0\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n",
    )
    .expect("write the descriptor");
    fs::create_dir(package.join("bundle")).expect("create the bundle directory");
    fs::write(package.join("bundle/index.md"), "# Index\n").expect("write the bundle");
    let out = plugin.dir("context-out");

    let output = plugin.xtask(&[
        "plugin",
        "build",
        "--dir",
        s(&package),
        "--output-dir",
        s(&out),
    ]);

    assert!(output.status.success(), "{}", stderr_of(&output));
    let archive = out.join("pohunek.context.fixture-0.2.0.tar.zst");
    let bytes = fs::read(&archive).expect("the archive is named by the descriptor");
    let digest = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8 stdout"),
        format!(
            "plugin build ok: pohunek.context.fixture 0.2.0 {digest} {}\n",
            archive.display()
        )
    );
}

#[test]
fn build_refuses_unusable_inputs_without_writing() {
    let plugin = Plugin::new();
    let out = plugin.dir("out");
    let no_package = plugin.dir("empty");

    let missing = plugin.path("missing");
    for (package, output_dir) in [(fixture_dir(), missing.clone()), (no_package, out.clone())] {
        let output = plugin.xtask(&[
            "plugin",
            "build",
            "--dir",
            s(&package),
            "--output-dir",
            s(&output_dir),
        ]);
        assert!(!output.status.success(), "{package:?} into {output_dir:?}");
    }
    assert!(!missing.exists());
    assert_eq!(fs::read_dir(&out).expect("read out").count(), 0);

    // An output directory inside the package directory would make the archive
    // an input of the next build.
    let inside = plugin.variant("inside", "version = \"0.1.0\"", "version = \"0.1.0\"");
    let nested = inside.join("out");
    fs::create_dir(&nested).expect("nested output");
    let output = plugin.xtask(&[
        "plugin",
        "build",
        "--dir",
        s(&inside),
        "--output-dir",
        s(&nested),
    ]);
    assert!(!output.status.success());
    assert_eq!(fs::read_dir(&nested).expect("read nested").count(), 0);

    // A symbolic link at the archive path is neither followed nor replaced.
    let victim = plugin.path("victim");
    fs::write(&victim, b"keep").expect("victim");
    symlink(&victim, out.join(ARCHIVE_NAME)).expect("link");
    let output = plugin.xtask(&[
        "plugin",
        "build",
        "--dir",
        s(&fixture_dir()),
        "--output-dir",
        s(&out),
    ]);
    assert!(!output.status.success());
    assert_eq!(fs::read(&victim).expect("victim"), b"keep");
}

#[test]
fn the_fixture_proves_in_isolation_and_leaves_nothing() {
    let plugin = Plugin::new();
    let (archive, digest) = plugin.build(&fixture_dir(), "built");
    let entry = plugin.entry("entry.toml", RUNTIME_ID, &digest);
    let evidence = plugin.path("evidence.json");

    // A decoy control socket at the caller's own runtime path: a proof that
    // reached the caller's daemon would connect to it.
    let decoy_path = plugin.env.runtime_dir().join("pohunek/daemon.sock");
    fs::create_dir_all(decoy_path.parent().expect("parent")).expect("decoy directory");
    let decoy = UnixListener::bind(&decoy_path).expect("bind the decoy");
    decoy.set_nonblocking(true).expect("nonblocking decoy");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("plugin prove ok: {PACKAGE_ID} {VERSION} {digest}\n")
    );

    let document = evidence_of(&evidence);
    let mut keys: Vec<&str> = document
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "core_version",
            "digest",
            "failed_step",
            "package_id",
            "platform",
            "result",
            "schema",
            "version"
        ]
    );
    assert_eq!(document["schema"], 1);
    assert_eq!(document["result"], "pass");
    assert!(document["failed_step"].is_null());
    assert_eq!(document["package_id"], PACKAGE_ID);
    assert_eq!(document["version"], VERSION);
    assert_eq!(document["digest"], digest);
    assert_eq!(
        document["platform"],
        host_platform().expect("platform name")
    );
    assert!(
        document["core_version"]
            .as_str()
            .is_some_and(|version| version.contains(env!("CARGO_PKG_VERSION"))),
        "{document}"
    );

    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
    assert!(
        matches!(decoy.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "the proof connected to the caller's control socket"
    );
}

#[test]
fn a_context_entry_proves_by_reading_the_package_back() {
    let plugin = Plugin::new();
    let package = plugin.context_package("context-package", "pohunek.context.fixture", "0.2.0");
    let (archive, digest) =
        plugin.build_as(&package, "built", "pohunek.context.fixture-0.2.0.tar.zst");
    let entry = plugin.context_entry(
        "context-entry.toml",
        "pohunek.context.fixture",
        &digest,
        "0.2.0",
    );
    let evidence = plugin.path("context-evidence.json");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("plugin prove ok: pohunek.context.fixture 0.2.0 {digest}\n")
    );

    let document = evidence_of(&evidence);
    assert_eq!(document["schema"], 1);
    assert_eq!(document["result"], "pass");
    assert!(document["failed_step"].is_null());
    assert_eq!(document["package_id"], "pohunek.context.fixture");
    assert_eq!(document["version"], "0.2.0");
    assert_eq!(document["digest"], digest);
    assert_eq!(document["kind"], "context");
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

#[test]
fn the_assistant_context_entry_proves_by_printing_its_prompt() {
    let plugin = Plugin::new();
    let package =
        plugin.context_package("assistant-package", "pohunek.context.assistant", "0.42.0");
    let (archive, digest) = plugin.build_as(
        &package,
        "built",
        "pohunek.context.assistant-0.42.0.tar.zst",
    );
    let entry = plugin.context_entry(
        "assistant-entry.toml",
        "pohunek.context.assistant",
        &digest,
        "0.42.0",
    );
    let evidence = plugin.path("assistant-evidence.json");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "pass");
    assert_eq!(document["kind"], "context");
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

#[test]
fn an_assistant_package_without_its_prompt_contract_fails_at_inspect() {
    let plugin = Plugin::new();
    let package = plugin.context_package_as(
        "damaged-assistant-package",
        "pohunek.context.assistant",
        "0.42.0",
        false,
    );
    let (archive, digest) = plugin.build_as(
        &package,
        "built",
        "pohunek.context.assistant-0.42.0.tar.zst",
    );
    let entry = plugin.context_entry(
        "damaged-assistant-entry.toml",
        "pohunek.context.assistant",
        &digest,
        "0.42.0",
    );
    let evidence = plugin.path("damaged-assistant-evidence.json");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "inspect");
    assert_eq!(document["kind"], "context");
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

#[test]
fn a_context_package_with_a_runtime_descriptor_fails_at_digest() {
    // Descriptor shape variants the digest step must refuse before any
    // prefix exists: one with a `[runtime]` table, one with no `[context]`
    // table. Each scenario keeps the real digest and identity, so only the
    // shape refusal can fail the proof.
    for (name, descriptor) in [
        (
            "runtime",
            "schema = 1\nid = \"pohunek.context.fixture\"\nversion = \"0.2.0\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[runtime]\nprogram = \"true\"\n\n[context]\ndir = \"bundle\"\n",
        ),
        (
            "no-context",
            "schema = 1\nid = \"pohunek.context.fixture\"\nversion = \"0.2.0\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n",
        ),
    ] {
        let plugin = Plugin::new();
        let package = plugin.context_package_with_descriptor(
            &format!("runtime-shaped-context-package-{name}"),
            "pohunek.context.fixture",
            "0.2.0",
            true,
            Some(descriptor),
        );
        let (archive, digest) = plugin.build_as(
            &package,
            "built",
            "pohunek.context.fixture-0.2.0.tar.zst",
        );
        let entry = plugin.context_entry(
            &format!("runtime-shaped-context-entry-{name}.toml"),
            "pohunek.context.fixture",
            &digest,
            "0.2.0",
        );
        let evidence = plugin.path(&format!("runtime-shaped-context-evidence-{name}.json"));
        let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
        assert_eq!(output.status.code(), Some(1), "{name}: {}", stderr_of(&output));
        let document = evidence_of(&evidence);
        assert_eq!(document["result"], "fail", "{name}");
        assert_eq!(document["failed_step"], "digest", "{name}");
        assert_eq!(document["kind"], "context", "{name}");
        assert_eq!(document["core_version"], Value::Null, "{name}");
        assert_eq!(plugin.leftover_roots(), Vec::<String>::new(), "{name}");
    }
}

/// Context identity: the entry carries the archive's digest but a foreign
/// package id or version, so the proof must fail at `digest` before any
/// prefix exists. The passing digest is the 0.2.0 build proved by
/// `a_context_entry_proves_by_reading_the_package_back`: only the entry
/// names a foreign package id or version, so the digest step's identity
/// comparison is the only check that can fail the proof.
#[test]
fn a_context_identity_mismatch_fails_at_digest_before_any_prefix_exists() {
    // The archive and its digest are the 0.2.0 build the passing proof
    // above reads back; only the entry names a foreign package id or
    // version, so the digest step's identity comparison is the only check
    // that can fail the proof.
    let plugin = Plugin::new();
    let package = plugin.context_package("identity-package", "pohunek.context.fixture", "0.2.0");
    let (archive, digest) =
        plugin.build_as(&package, "built", "pohunek.context.fixture-0.2.0.tar.zst");
    for (name, package_id, version) in [
        ("package", "pohunek.context.other", "0.2.0"),
        ("version", "pohunek.context.fixture", "0.3.0"),
    ] {
        let entry = plugin.context_entry(
            &format!("identity-{name}-entry.toml"),
            package_id,
            &digest,
            version,
        );
        let evidence = plugin.path(&format!("identity-{name}-evidence.json"));
        let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name}: {}",
            stderr_of(&output)
        );
        let document = evidence_of(&evidence);
        assert_eq!(document["result"], "fail", "{name}");
        assert_eq!(document["failed_step"], "digest", "{name}");
        assert_eq!(document["kind"], "context", "{name}");
        assert_eq!(document["core_version"], Value::Null, "{name}");
        assert_eq!(plugin.leftover_roots(), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn a_context_archive_mismatch_fails_at_digest_before_any_prefix_exists() {
    let plugin = Plugin::new();
    let package = plugin.context_package("context-package", "pohunek.context.fixture", "0.2.0");
    let (archive, _digest) =
        plugin.build_as(&package, "built", "pohunek.context.fixture-0.2.0.tar.zst");
    let entry = plugin.context_entry(
        "context-entry.toml",
        "pohunek.context.fixture",
        ABSENT_DIGEST,
        "0.2.0",
    );
    let evidence = plugin.path("context-evidence.json");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "digest");
    assert_eq!(document["kind"], "context");
    assert_eq!(document["core_version"], Value::Null);
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

/// A `--no-enable` install leaves the context package disabled but still
/// selected, so the proof must fail at `inspect` when the predicate
/// refuses the disabled install.
#[test]
fn a_disabled_context_install_fails_at_inspect() {
    let plugin = Plugin::new();
    let package = plugin.context_package("disabled-package", "pohunek.context.fixture", "0.2.0");
    let (archive, digest) =
        plugin.build_as(&package, "built", "pohunek.context.fixture-0.2.0.tar.zst");
    let entry = plugin.context_entry(
        "disabled-context-entry.toml",
        "pohunek.context.fixture",
        &digest,
        "0.2.0",
    );
    let evidence = plugin.path("disabled-context-evidence.json");

    let output = plugin.prove_no_enable(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "inspect");
    assert_eq!(document["kind"], "context");
    assert!(document["core_version"].is_string());
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

/// A `--pre-install` archive with a foreign package id is refused at
/// `install` before anything is installed: only another version of the
/// proved package may stage the unselected state.
#[test]
fn a_foreign_pre_install_archive_is_refused_at_install() {
    let plugin = Plugin::new();
    let package = plugin.context_package("foreign-older", "pohunek.context.fixture", "0.2.0");
    let (archive, digest) =
        plugin.build_as(&package, "built", "pohunek.context.fixture-0.2.0.tar.zst");
    let entry = plugin.context_entry(
        "foreign-context-entry.toml",
        "pohunek.context.fixture",
        &digest,
        "0.2.0",
    );
    let foreign = plugin.context_package("foreign-newer", "pohunek.context.other", "0.3.0");
    let (pre_archive, _pre_digest) = plugin.build_as(
        &foreign,
        "foreign-newer-out",
        "pohunek.context.other-0.3.0.tar.zst",
    );
    let evidence = plugin.path("foreign-pre-install-evidence.json");
    let output = plugin.prove_with_pre_install(
        &archive,
        &entry,
        &release_dir(),
        &evidence,
        &pre_archive,
        false,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    assert!(
        stderr_of(&output).contains("the pre-installed archive names"),
        "the refusal names the binding archive: {}",
        stderr_of(&output)
    );
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "install");
    assert_eq!(document["kind"], "context");
    assert!(document["core_version"].is_string());
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

/// A `--pre-install` file over the input cap is refused before it is
/// decoded: the capped read bounds a caller-controlled file.
#[test]
fn an_oversized_pre_install_archive_is_refused_before_it_is_decoded() {
    use std::io::Write as _;
    let plugin = Plugin::new();
    let package = plugin.context_package("oversized-older", "pohunek.context.fixture", "0.2.0");
    let (archive, digest) =
        plugin.build_as(&package, "built", "pohunek.context.fixture-0.2.0.tar.zst");
    let entry = plugin.context_entry(
        "oversized-context-entry.toml",
        "pohunek.context.fixture",
        &digest,
        "0.2.0",
    );
    let oversized = plugin.path("oversized-pre-install.tar.zst");
    let mut file = fs::File::create(&oversized).expect("create the oversized pre-install");
    file.write_all(&vec![
        0u8;
        package::Limits::DEFAULT.max_compressed_bytes as usize
            + 1
    ])
    .expect("write over the input cap");
    let evidence = plugin.path("oversized-pre-install-evidence.json");
    let output = plugin.prove_with_pre_install(
        &archive,
        &entry,
        &release_dir(),
        &evidence,
        &oversized,
        false,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let limit = package::Limits::DEFAULT.max_compressed_bytes;
    assert!(
        stderr_of(&output).contains(&format!("is larger than the {limit}-byte limit")),
        "the refusal names the input cap: {}",
        stderr_of(&output)
    );
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "install");
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

/// A newer installed version of the same package leaves the proved context
/// version unselected, so the proof must fail at `install` on the default
/// path and at `inspect` under `--no-enable`, where the digest check refuses
/// the other, selected version. Each new version needs its
/// own one-entry trust, so the pre-installed archive carries its own entry
/// and is installed through a digest trust.
fn unselected_proof_setup(plugin: &Plugin) -> (PathBuf, PathBuf, PathBuf) {
    let older = plugin.context_package("unselected-older", "pohunek.context.fixture", "0.2.0");
    let (archive, digest) = plugin.build_as(
        &older,
        "unselected-older-out",
        "pohunek.context.fixture-0.2.0.tar.zst",
    );
    let newer = plugin.context_package("unselected-newer", "pohunek.context.fixture", "0.3.0");
    let (pre_archive, _pre_digest) = plugin.build_as(
        &newer,
        "unselected-newer-out",
        "pohunek.context.fixture-0.3.0.tar.zst",
    );
    let entry = plugin.context_entry(
        "unselected-context-entry.toml",
        "pohunek.context.fixture",
        &digest,
        "0.2.0",
    );
    (archive, entry, pre_archive)
}

#[test]
fn an_unselected_context_install_fails_at_install() {
    let plugin = Plugin::new();
    let (archive, entry, pre_archive) = unselected_proof_setup(&plugin);
    let evidence = plugin.path("unselected-context-evidence.json");
    let output = plugin.prove_with_pre_install(
        &archive,
        &entry,
        &release_dir(),
        &evidence,
        &pre_archive,
        false,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "install");
    assert_eq!(document["kind"], "context");
    assert!(document["core_version"].is_string());
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

#[test]
fn an_unselected_context_install_fails_at_inspect_when_not_enabled() {
    let plugin = Plugin::new();
    let (archive, entry, pre_archive) = unselected_proof_setup(&plugin);
    let evidence = plugin.path("unselected-no-enable-context-evidence.json");
    let output = plugin.prove_with_pre_install(
        &archive,
        &entry,
        &release_dir(),
        &evidence,
        &pre_archive,
        true,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "inspect");
    assert_eq!(document["kind"], "context");
    assert!(document["core_version"].is_string());
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

#[test]
fn the_runtime_program_is_never_launched() {
    let plugin = Plugin::new();
    let marker = plugin.path("launched");
    let program = plugin.path("program.sh");
    fs::write(
        &program,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .expect("program");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("program mode");
    let package = plugin.variant(
        "marker-package",
        "program = \"true\"",
        &format!("program = \"{}\"", program.display()),
    );
    let (archive, digest) = plugin.build(&package, "built");
    let entry = plugin.entry("entry.toml", RUNTIME_ID, &digest);
    let evidence = plugin.path("evidence.json");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(evidence_of(&evidence)["result"], "pass");
    assert!(!marker.exists(), "the proof launched the runtime program");
}

#[test]
fn a_runtime_id_the_index_never_authorizes_fails_at_install() {
    let plugin = Plugin::new();
    let package = plugin.variant("codex-package", "id = \"kitfixture\"", "id = \"codex\"");
    let (archive, digest) = plugin.build(&package, "built");
    let entry = plugin.entry("entry.toml", "codex", &digest);
    let evidence = plugin.path("evidence.json");

    let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    assert!(
        stderr_of(&output).contains("package_runtime_not_claimable"),
        "the install-step refusal names the fixture: {}",
        stderr_of(&output)
    );
    let document = evidence_of(&evidence);
    assert_eq!(document["result"], "fail");
    assert_eq!(document["failed_step"], "install");
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}

#[test]
fn a_differing_digest_or_identity_fails_at_digest_before_any_prefix_exists() {
    let plugin = Plugin::new();
    let (archive, digest) = plugin.build(&fixture_dir(), "built");

    for (name, entry) in [
        (
            "digest",
            plugin.entry("digest.toml", RUNTIME_ID, ABSENT_DIGEST),
        ),
        ("identity", plugin.entry("identity.toml", "other", &digest)),
    ] {
        let evidence = plugin.path(&format!("{name}.json"));
        let output = plugin.prove(&archive, &entry, &release_dir(), &evidence);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name}: {}",
            stderr_of(&output)
        );
        let document = evidence_of(&evidence);
        assert_eq!(document["result"], "fail", "{name}");
        assert_eq!(document["failed_step"], "digest", "{name}");
        assert_eq!(document["core_version"], Value::Null, "{name}");
        assert_eq!(plugin.leftover_roots(), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn a_release_directory_that_cannot_run_fails_at_host_and_is_removed() {
    let plugin = Plugin::new();
    let (archive, digest) = plugin.build(&fixture_dir(), "built");
    let entry = plugin.entry("entry.toml", RUNTIME_ID, &digest);

    // A daemon that exits at once is not ready; one missing binary is refused.
    let exits = plugin.dir("exits");
    let missing = plugin.dir("missing");
    for name in ["pohunek", "pohunekd", "pohunek-sessiond"] {
        let script = "#!/bin/sh\nexit 3\n";
        // The daemon answers `--version` and then exits: it is copied and
        // started, but never ready.
        let daemon_script =
            "#!/bin/sh\n[ \"$1\" = --version ] && echo 'pohunekd 0' && exit 0\nexit 3\n";
        fs::write(
            exits.join(name),
            if name == "pohunekd" {
                daemon_script
            } else {
                script
            },
        )
        .expect("fake binary");
        fs::set_permissions(exits.join(name), fs::Permissions::from_mode(0o755)).expect("mode");
        if name != "pohunekd" {
            fs::write(missing.join(name), script).expect("fake binary");
            fs::set_permissions(missing.join(name), fs::Permissions::from_mode(0o755))
                .expect("mode");
        }
    }
    for (name, release) in [("exits", exits), ("missing", missing)] {
        let evidence = plugin.path(&format!("{name}.json"));
        let output = plugin.prove(&archive, &entry, &release, &evidence);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name}: {}",
            stderr_of(&output)
        );
        let document = evidence_of(&evidence);
        assert_eq!(document["failed_step"], "host", "{name}");
        assert_eq!(plugin.leftover_roots(), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn a_proof_that_cannot_run_writes_no_evidence_and_keeps_an_existing_one() {
    let plugin = Plugin::new();
    let (archive, digest) = plugin.build(&fixture_dir(), "built");
    let entry = plugin.entry("entry.toml", RUNTIME_ID, &digest);

    let existing = plugin.path("existing.json");
    fs::write(&existing, b"keep").expect("existing evidence");
    let output = plugin.prove(&archive, &entry, &release_dir(), &existing);
    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert_eq!(fs::read(&existing).expect("existing evidence"), b"keep");

    let absent = plugin.path("absent.json");
    let output = plugin.prove(
        &plugin.path("no-such-archive"),
        &entry,
        &release_dir(),
        &absent,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert!(!absent.exists());
    assert_eq!(plugin.leftover_roots(), Vec::<String>::new());
}
