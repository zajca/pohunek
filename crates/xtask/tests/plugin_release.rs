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
//!   digest or identity at `digest`, a missing or exiting daemon at `host`;
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

/// Prefix of the directory a proof creates in `TMPDIR`.
const PROOF_ROOT_PREFIX: &str = "pohunek-prove-";

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
        (out.join(ARCHIVE_NAME), fields[5].to_owned())
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
        "{}",
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
