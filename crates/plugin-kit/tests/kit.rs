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

fn context_fixture_dir() -> PathBuf {
    pohunek_test_support::manifest_dir().join("tests/fixtures/kit-context-package")
}

/// Marks a program executable on Unix; a no-op elsewhere (the kit reads the
/// archive executable bit the test harness sets on Unix only).
#[cfg(unix)]
fn make_executable(program: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(program, fs::Permissions::from_mode(0o755))
        .expect("make the program executable");
}

/// Marks a program executable on Unix; a no-op elsewhere (the kit reads the
/// archive executable bit the test harness sets on Unix only).
#[cfg(not(unix))]
fn make_executable(_program: &Path) {}

/// Copies a directory tree: used to mutate a fixture copy without touching
/// the fixture itself.
fn copy_dir(from: &Path, to: &Path) {
    let mut stack = vec![from.to_path_buf()];
    while let Some(source) = stack.pop() {
        let relative = source.strip_prefix(from).expect("below the source");
        let target = to.join(relative);
        if source.is_dir() {
            fs::create_dir_all(&target).expect("create the directory");
            for entry in fs::read_dir(&source).expect("read the directory") {
                stack.push(entry.expect("read the entry").path());
            }
        } else {
            fs::copy(&source, &target).expect("copy the file");
        }
    }
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
    /// Parent for the kit's isolation roots: the temporary root itself.
    /// The kit names its roots `pk-` plus 6 random characters, and the
    /// nested worker socket paths fit only below a parent this short; a
    /// deeper parent (like the `TestEnv` tmp directory) pushes the kit
    /// onto its short fallback instead. The base is shared with the other
    /// tests, but the root names are unique: every scenario asserts only
    /// the root its own run names, so a foreign temporary directory never
    /// fails a scenario.
    root_parent: PathBuf,
}

impl Kit {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        let marker = env.cwd().join("crates/xtask/Cargo.toml");
        fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
        fs::write(marker, "").expect("marker");
        let root_parent = pohunek_test_support::temp_root();
        Self { env, root_parent }
    }

    /// The kit with its `TMPDIR` below a long directory: the socket check
    /// rejects the temp root and the kit falls back to its short parent.
    fn with_long_tmpdir(name: &str) -> Self {
        let mut kit = Self::new();
        kit.root_parent = kit.dir(name);
        kit
    }

    fn path(&self, name: &str) -> PathBuf {
        self.env.root().join(name)
    }

    /// The cwd the harness gives the kit binary: the smoke script must
    /// run in this directory, not in the isolated root.
    fn cwd(&self) -> &Path {
        self.env.cwd()
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        fs::create_dir(&path).expect("create the directory");
        path
    }

    /// Runs the kit binary with every wait bounded by `HANG_GUARD`: a hung
    /// kit is killed and the failure names its stderr. Both pipes drain on
    /// reader threads, so a kit that writes more than the pipe buffer
    /// cannot block on output.
    fn kit(&self, args: &[&str]) -> Output {
        use std::io::Read as _;
        use std::process::Stdio;
        use std::sync::mpsc;
        use std::time::Instant;
        let mut child = self
            .env
            .command(pohunek_test_support::bin_exe("pohunek-plugin-kit"))
            .current_dir(self.env.cwd())
            .env("TMPDIR", &self.root_parent)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run pohunek-plugin-kit");
        let (sender, receiver) = mpsc::channel();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let stdout_sender = sender.clone();
        std::thread::spawn(move || {
            let mut output = Vec::new();
            if let Some(mut pipe) = stdout.take() {
                let _ = pipe.read_to_end(&mut output);
            }
            let _ = stdout_sender.send(("stdout", output));
        });
        std::thread::spawn(move || {
            let mut output = Vec::new();
            if let Some(mut pipe) = stderr.take() {
                let _ = pipe.read_to_end(&mut output);
            }
            let _ = sender.send(("stderr", output));
        });
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            match child.try_wait().expect("poll the kit") {
                Some(status) => {
                    let _ = child.wait();
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    for (name, output) in receiver {
                        if name == "stdout" {
                            stdout = output;
                        } else {
                            stderr = output;
                        }
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
                    let mut stderr = Vec::new();
                    for (name, output) in receiver {
                        if name == "stderr" {
                            stderr = output;
                        }
                    }
                    panic!(
                        "pohunek-plugin-kit {args:?} hung past {HANG_GUARD:?}: {}",
                        String::from_utf8_lossy(&stderr)
                    );
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

    /// An entry file for `archive` with the given identity.
    fn entry(&self, name: &str, digest: &str) -> PathBuf {
        self.entry_as(name, PACKAGE_ID, digest, VERSION)
    }

    /// An entry file for an archive with the given identity.
    fn entry_as(&self, name: &str, package_id: &str, digest: &str, version: &str) -> PathBuf {
        let text = format!(
            "package_id = \"{package_id}\"\n\
             runtime_id = \"kitfixture\"\n\
             version = \"{version}\"\n\
             digest = \"{digest}\"\n\
             url = \"https://localhost/kit-scenario\"\n\
             platforms = [\"{}\"]\n\
             core = \">=0.0.0\"\n\n\
             [source]\n\
             repo = \"local/prove\"\n\
             repo_id = 1\n\
             tag = \"v{version}\"\n\
             commit = \"0000000000000000000000000000000000000000\"\n\
             built_with = \"v0.0.0\"\n",
            package::host_platform().expect("this platform has an index platform name")
        );
        let path = self.path(name);
        fs::write(&path, text).expect("write the entry");
        path
    }

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
}

fn evidence_of(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("the evidence file exists"))
        .expect("the evidence is JSON")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The short fallback parent as the platform resolves it: on macOS `/tmp`
/// is a symbolic link to `/private/tmp`, while the kit reports the
/// canonical root it created.
fn short_fallback() -> PathBuf {
    fs::canonicalize(plugin_kit::isolated::SHORT_TEMP_PARENT).expect("the short parent resolves")
}

/// The isolation root a passing run names in its `--json` document: the
/// kit legitimately puts it below the temp root or below its short
/// fallback when the temp root is too long for a socket path. Either way
/// the run removes it before the report.
fn used_root(document: &Value, kit: &Kit) -> PathBuf {
    let root = PathBuf::from(document["root"].as_str().expect("the used root"));
    assert!(
        root.starts_with(&kit.root_parent) || root.starts_with(short_fallback()),
        "the run used its temp root or the short fallback: {}",
        root.display()
    );
    assert!(!root.exists(), "the run removed its root");
    root
}

/// Whether the background pid the smoke script recorded is gone: absent
/// from `/proc`, or a zombie (`Z` as the state after the `comm` field in
/// `/proc/<pid>/stat`). A process the kit group-killed stays a zombie
/// until its parent is reaped, so the zombie state counts as gone too.
fn background_child_is_gone(pid: i32) -> bool {
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let end = stat.iter().rposition(|byte| *byte == b')');
    let Some(end) = end else { return false };
    stat.get(end + 2..end + 3) == Some(&b"Z"[..])
}

/// The isolation root a failed run names on its stderr. The run names the
/// root exactly once; the named directory must already be gone unless the
/// removal itself is the failure. The scenario owns this name and no
/// foreign root can fail it.
fn named_root(output: &Output) -> PathBuf {
    let mut named: Vec<String> = stderr_of(output)
        .lines()
        .filter_map(|line| line.strip_prefix("isolation root: ").map(str::to_owned))
        .collect();
    assert_eq!(
        named.len(),
        1,
        "the failure names its isolation root exactly once: {}",
        stderr_of(output)
    );
    let root = PathBuf::from(named.pop().expect("one named root"));
    assert!(
        !root.exists(),
        "a failed run removes its root before the report"
    );
    root
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
        "--json",
    ]);
    assert!(
        output.status.success(),
        "smoke failed: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["package_id"].as_str(), Some(PACKAGE_ID));
    assert_eq!(document["version"].as_str(), Some(VERSION));
    assert_eq!(document["activity"].as_str(), Some("idle"));
    assert!(
        document["screen_lines"].as_u64().unwrap_or(0) > 0,
        "the smoke saw a non-empty screen: {document:?}"
    );
    assert_eq!(
        document["detection_timeout_s"],
        serde_json::json!(60),
        "a run without the flag reports the 60 s default detection bound"
    );
    used_root(&document, &kit);
}

#[test]
fn kit_smoke_runs_the_script_against_the_live_session() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let script = kit.path("smoke-script.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             set -eu\n\
             echo \"a line on the script's own stdout\"\n\
             pwd > \"{}\"\n\
             pohunek session screen \"$POHUNEK_SMOKE_SESSION\" --json | grep -q ready\n",
            kit.path("script-cwd.txt").display()
        ),
    )
    .expect("write the smoke script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "smoke with a script failed: {}",
        stderr_of(&output)
    );
    // The script's stdout line went to the kit's stderr, so the kit's
    // stdout stays the single JSON document the smoke promises.
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert!(
        stderr_of(&output).contains("a line on the script's own stdout"),
        "the script line is on the kit stderr: {}",
        stderr_of(&output)
    );
    // The script ran in the smoke's own cwd (the directory the caller
    // started the kit in: the workflow runs the smoke step from the
    // plugin repository), not in the isolated root.
    let script_cwd = PathBuf::from(
        fs::read_to_string(kit.path("script-cwd.txt"))
            .expect("the script recorded its cwd")
            .trim(),
    );
    assert_eq!(
        script_cwd,
        kit.cwd(),
        "the smoke script runs in the kit's own cwd"
    );
    assert_ne!(
        script_cwd,
        used_root(&document, &kit),
        "the smoke script does not run inside the isolated root"
    );
    used_root(&document, &kit);
}

#[test]
fn kit_smoke_skips_the_script_when_the_screen_check_fails() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    // A package whose program prints nothing the detector knows: the
    // screen check times out, so the smoke must fail without starting
    // the script, and still tear the root down.
    let text = fs::read_to_string(fixture_dir().join("runtime.toml")).expect("read the fixture");
    let text = text.replace(
        "args = [\"-c\", \"echo ready; exec sleep 300\"]",
        "args = [\"-c\", \"exec sleep 300\"]",
    );
    assert!(
        !text.contains("echo ready"),
        "the variant prints nothing the detector knows"
    );
    let package = kit.dir("quiet-package");
    fs::write(package.join("runtime.toml"), text).expect("write the variant");
    fs::copy(
        fixture_dir().join("detect.toml"),
        package.join("detect.toml"),
    )
    .expect("copy the file");
    for screen in ["idle_prompt.txt", "blocked_input.txt"] {
        fs::copy(
            fixture_dir().join("screens").join(screen),
            package.join(screen),
        )
        .expect("copy the screen");
    }
    let untouched = kit.path("script-ran.txt");
    let script = kit.path("smoke-quiet.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nset -eu\ntouch \"{}\"\n", untouched.display()),
    )
    .expect("write the smoke script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&package),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
        // The quiet package never prints what the detector knows, so the
        // 1 s bound always expires and host speed cannot change the outcome.
        "--detection-timeout",
        "1",
        "--json",
    ]);
    assert!(
        !output.status.success(),
        "smoke with an unknown screen passed: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("the detector stayed `unknown` for 1s"),
        "the screen check failed first: {}",
        stderr_of(&output)
    );
    assert!(
        !untouched.exists(),
        "the script did not start after the failed check"
    );
    // `named_root` asserts the failed run removed its root.
    named_root(&output);
}

#[test]
fn kit_smoke_kills_the_background_child_of_the_script() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let pids = kit.path("pids.txt");
    let script = kit.path("smoke-background.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -eu\n( exec sleep 300 ) &\necho $! > \"{}\"\npohunek session screen \"$POHUNEK_SMOKE_SESSION\" --json | grep -q ready\n",
            pids.display()
        ),
    )
    .expect("write the smoke script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "smoke with a backgrounding script failed: {}",
        stderr_of(&output)
    );
    let backgrounded: i32 = fs::read_to_string(&pids)
        .expect("the script recorded its background child")
        .trim()
        .parse()
        .expect("the recorded pid parses");
    // The kit kills the script's process group after the script exits.
    // The background child's stdout and stderr are the kit's stderr,
    // so `output()` above returns only after every writer of that
    // pipe has closed: the child has exited by then. An exited child
    // is either reaped (absent from `/proc`) or a zombie (`Z` in
    // `/proc/<pid>/stat`); both count as gone. Without the kit's kill
    // this scenario never returns: the open pipe keeps `output()`
    // blocked until the harness times the test out, so the revert
    // fails by the timeout.
    assert!(
        background_child_is_gone(backgrounded),
        "the script's background child is gone after the smoke"
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    used_root(&document, &kit);
}

#[test]
fn kit_smoke_refuses_a_failing_script() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let script = kit.path("smoke-fail.sh");
    fs::write(&script, "#!/bin/sh\nexit 3\n").expect("write the failing script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
    ]);
    assert!(
        !output.status.success(),
        "a failing script must fail the smoke"
    );
    assert!(
        stderr_of(&output).contains(&format!("the smoke script `{}` exited 3", script.display())),
        "the error names the script and its exit: {}",
        stderr_of(&output)
    );
    named_root(&output);
}

#[test]
fn kit_smoke_reports_the_effective_script_timeout() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let script = kit.path("smoke-timeout.sh");
    fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write the script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    for (args, expected) in [(vec![], 60_u64), (vec!["--script-timeout", "120"], 120_u64)] {
        let mut argv = vec![
            "smoke".to_owned(),
            "--dir".to_owned(),
            s(&fixture_dir()).to_owned(),
            "--release-dir".to_owned(),
            s(&release).to_owned(),
            "--script".to_owned(),
            s(&script).to_owned(),
            "--json".to_owned(),
        ];
        argv.extend(args.iter().map(ToString::to_string));
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let output = kit.kit(&refs);
        assert!(
            output.status.success(),
            "smoke with a script failed: {}",
            stderr_of(&output)
        );
        let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
        assert_eq!(
            document["script_timeout_s"],
            serde_json::json!(expected),
            "the report carries the effective script bound"
        );
        used_root(&document, &kit);
    }
}

// One script with a known, bounded run time proves both directions of
// the configurable bound: 3 s of sleep fails under `--script-timeout 1`
// and passes under `--script-timeout 20`, with 17 s of headroom for a
// loaded runner (well under HANG_GUARD's 120 s).
#[test]
fn kit_smoke_bounds_a_slow_script_by_its_timeout() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let script = kit.path("smoke-slow.sh");
    fs::write(&script, "#!/bin/sh\nsleep 3\nexit 0\n").expect("write the slow script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    let short = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
        "--script-timeout",
        "1",
    ]);
    assert!(
        !short.status.success(),
        "a script past its bound must fail the smoke"
    );
    assert!(
        stderr_of(&short).contains(&format!(
            "smoke script {} did not exit within 1 s",
            script.display()
        )),
        "the timeout error names the script and the bound: {}",
        stderr_of(&short)
    );
    named_root(&short);
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
        "--script-timeout",
        "20",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "smoke with a raised script bound failed: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        document["script_timeout_s"],
        serde_json::json!(20_u64),
        "the report carries the raised script bound"
    );
    used_root(&document, &kit);
}

#[test]
fn kit_smoke_fails_when_the_script_outlives_its_bound() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let script = kit.path("smoke-hang.sh");
    fs::write(&script, "#!/bin/sh\nexec sleep 300\n").expect("write the hanging script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script",
        s(&script),
        "--script-timeout",
        "1",
    ]);
    assert!(
        !output.status.success(),
        "a script past its bound must fail the smoke"
    );
    assert!(
        stderr_of(&output).contains(&format!(
            "smoke script {} did not exit within 1 s",
            script.display()
        )),
        "the timeout error names the script and the bound: {}",
        stderr_of(&output)
    );
    named_root(&output);
}

#[test]
fn kit_smoke_refuses_a_script_timeout_outside_its_range() {
    use std::os::unix::fs::PermissionsExt as _;
    let kit = Kit::new();
    let release = release_dir();
    let script = kit.path("smoke-range.sh");
    fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write the script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod the script");
    for bound in ["0", "901"] {
        let output = kit.kit(&[
            "smoke",
            "--dir",
            s(&fixture_dir()),
            "--release-dir",
            s(&release),
            "--script",
            s(&script),
            "--script-timeout",
            bound,
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "rc is 2 for --script-timeout {bound}: {}",
            stderr_of(&output)
        );
        assert!(
            stderr_of(&output).contains("--script-timeout must be 1..=900 seconds"),
            "the refusal names the range: {}",
            stderr_of(&output)
        );
    }
}

#[test]
fn kit_smoke_refuses_a_detection_timeout_outside_its_range() {
    let kit = Kit::new();
    let release = release_dir();
    for bound in ["0", "61"] {
        let output = kit.kit(&[
            "smoke",
            "--dir",
            s(&fixture_dir()),
            "--release-dir",
            s(&release),
            "--detection-timeout",
            bound,
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "rc is 2 for --detection-timeout {bound}: {}",
            stderr_of(&output)
        );
        assert!(
            stderr_of(&output).contains("--detection-timeout must be 1..=60 seconds"),
            "the refusal names the range: {}",
            stderr_of(&output)
        );
    }
}

#[test]
fn kit_smoke_refuses_a_script_timeout_without_a_script() {
    let kit = Kit::new();
    let release = release_dir();
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--script-timeout",
        "120",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("--script-timeout needs --script"),
        "the refusal names the missing script: {}",
        stderr_of(&output)
    );
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
        "--json",
    ]);
    assert!(
        output.status.success(),
        "prove failed: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["package_id"].as_str(), Some(PACKAGE_ID));
    assert_eq!(document["version"].as_str(), Some(VERSION));
    assert!(
        document["digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "the report names the archive digest: {document:?}"
    );
    used_root(&document, &kit);
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
}

#[test]
fn kit_prove_archive_proves_the_entry_asset() {
    let kit = Kit::new();
    let release = release_dir();
    let archive = kit.build(&fixture_dir(), "out");
    let digest = digest_of(&archive);
    let entry = kit.entry("entry.toml", &digest);

    let evidence = kit.path("prove-archive.json");
    let output = kit.kit(&[
        "prove",
        "--archive",
        s(&archive),
        "--entry",
        s(&entry),
        "--release-dir",
        s(&release),
        "--evidence",
        s(&evidence),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "prove --archive failed: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let line = document["line"].as_str().expect("the line");
    assert!(
        line.starts_with(&format!("plugin prove ok: {PACKAGE_ID} {VERSION} sha256:")),
        "unexpected prove line: {line:?}"
    );
    used_root(&document, &kit);
    let evidence = evidence_of(&evidence);
    assert_eq!(evidence["result"], "pass");
    assert_eq!(evidence["package_id"], PACKAGE_ID);
    assert_eq!(
        evidence["kit_sha256"].as_str(),
        Some(digest_of(&pohunek_test_support::bin_exe("pohunek-plugin-kit")).as_str())
    );

    let output = kit.kit(&[
        "prove",
        "--dir",
        s(&fixture_dir()),
        "--archive",
        s(&archive),
        "--entry",
        s(&entry),
        "--release-dir",
        s(&release),
        "--evidence",
        s(&kit.path("prove-clash.json")),
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("prove needs `--dir`, or `--archive` with `--entry`"),
        "the refusal names the modes: {}",
        stderr_of(&output)
    );
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

const CONTEXT_PACKAGE_ID: &str = "pohunek.context.kitfixture";

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
fn kit_lint_passes_a_context_package_and_reports_context() {
    let kit = Kit::new();
    let package = context_fixture_dir();
    let output = kit.kit(&["lint", "--dir", s(&package)]);
    assert!(
        output.status.success(),
        "context lint failed: {}",
        stderr_of(&output)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8 stdout"),
        format!("plugin lint ok: {CONTEXT_PACKAGE_ID} {VERSION} context\n")
    );
    let output = kit.kit(&["lint", "--dir", s(&package), "--json"]);
    assert!(
        output.status.success(),
        "context lint --json failed: {}",
        stderr_of(&output)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("lint --json reports JSON");
    assert_eq!(report["kind"], Value::String("context".to_owned()));
    assert_eq!(
        report["package_id"],
        Value::String(CONTEXT_PACKAGE_ID.to_owned())
    );
    assert!(
        report.get("runtime_id").is_none(),
        "a context lint names no runtime id: {report}"
    );
    let archive = kit.build(&package, "context-out");
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
        "context verify failed: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_lint_refuses_a_context_package_without_the_context_feature() {
    let kit = Kit::new();
    let dir = kit.dir("context-without-feature");
    copy_dir(&context_fixture_dir(), &dir);
    let manifest = fs::read_to_string(dir.join("runtime.toml")).expect("read the manifest");
    let manifest = manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with("requires"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(dir.join("runtime.toml"), manifest).expect("drop requires");
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("package-context-v1"),
        "the refusal names the missing context feature: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_lint_refuses_a_context_package_with_a_missing_dir() {
    let kit = Kit::new();
    let dir = kit.dir("context-missing-dir");
    copy_dir(&context_fixture_dir(), &dir);
    let manifest = fs::read_to_string(dir.join("runtime.toml")).expect("read the manifest");
    let manifest = manifest.replace("dir = \"bundle\"", "dir = \"absent\"");
    assert!(
        manifest.contains("dir = \"absent\""),
        "the fixture names the bundle dir"
    );
    fs::write(dir.join("runtime.toml"), manifest).expect("point dir at nothing");
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("context.dir"),
        "the refusal names the missing context dir: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_lint_refuses_a_package_with_neither_table() {
    let kit = Kit::new();
    let dir = kit.dir("neither-table");
    copy_dir(&context_fixture_dir(), &dir);
    let manifest = fs::read_to_string(dir.join("runtime.toml")).expect("read the manifest");
    let manifest = manifest
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("[context]") && !trimmed.starts_with("dir =")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(
        !manifest.contains("[context]"),
        "the variant drops the context table"
    );
    fs::write(dir.join("runtime.toml"), manifest).expect("drop the context table");
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
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
    assert!(
        stderr_of(&output).contains("neither a runtime nor commands"),
        "the refusal names the classify reason: {}",
        stderr_of(&output)
    );
}

/// Package id of the commands-only packages the scenarios write.
const COMMANDS_PACKAGE_ID: &str = "pohunek.commands.kitfixture";

/// Writes a commands-only package named `name` below the kit's temp root: one
/// command `host-state` running `bin/fleet-state` with `body` as its script.
/// `extra_manifest` is appended to the descriptor.
fn write_commands_package(
    kit: &Kit,
    name: &str,
    requires: &str,
    extra_manifest: &str,
    body: &str,
) -> PathBuf {
    let dir = kit.dir(name);
    fs::write(
        dir.join("runtime.toml"),
        format!(
            "schema = 1\nid = \"{COMMANDS_PACKAGE_ID}\"\nversion = \"{VERSION}\"\nruntime_api = 1\n{requires}\n[commands.host-state]\nprogram = \"bin/fleet-state\"\nargs = [\"--json\"]\nexpose = \"peers\"\n{extra_manifest}"
        ),
    )
    .expect("write the commands-only manifest");
    let bin = dir.join("bin");
    fs::create_dir(&bin).expect("create the program directory");
    let program = bin.join("fleet-state");
    fs::write(&program, body).expect("write the program");
    make_executable(&program);
    dir
}

const COMMANDS_REQUIRES: &str = "requires = [\"package-commands-v1\"]\n";

#[test]
fn kit_lint_passes_a_commands_only_package_and_reports_commands() {
    let kit = Kit::new();
    let dir = write_commands_package(
        &kit,
        "commands-only",
        COMMANDS_REQUIRES,
        "",
        "#!/bin/sh\necho ok\n",
    );
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
    assert!(
        output.status.success(),
        "commands lint failed: {}",
        stderr_of(&output)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8 stdout"),
        format!("plugin lint ok: {COMMANDS_PACKAGE_ID} {VERSION} commands\n")
    );
    let output = kit.kit(&["lint", "--dir", s(&dir), "--json"]);
    assert!(
        output.status.success(),
        "commands lint --json failed: {}",
        stderr_of(&output)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("lint --json reports JSON");
    assert_eq!(report["kind"], Value::String("commands".to_owned()));
    assert!(
        report.get("runtime_id").is_none(),
        "a commands lint names no runtime id: {report}"
    );
    let archive = kit.build(&dir, "commands-out");
    let output = kit.kit(&[
        "verify",
        "--dir",
        s(&dir),
        "--archive",
        s(&archive),
        "--sha256",
        &digest_of(&archive),
    ]);
    assert!(
        output.status.success(),
        "commands verify failed: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_lint_refuses_a_commands_package_without_the_commands_feature() {
    let kit = Kit::new();
    let dir = write_commands_package(&kit, "commands-no-feature", "", "", "#!/bin/sh\necho ok\n");
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("package-commands-v1"),
        "the refusal names the missing feature: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_lint_refuses_a_commands_package_with_a_missing_program() {
    let kit = Kit::new();
    let dir = write_commands_package(
        &kit,
        "commands-missing-program",
        COMMANDS_REQUIRES,
        "",
        "#!/bin/sh\necho ok\n",
    );
    fs::remove_file(dir.join("bin/fleet-state")).expect("remove the program");
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
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
fn kit_lint_refuses_commands_combined_with_a_context() {
    let kit = Kit::new();
    let dir = write_commands_package(
        &kit,
        "commands-and-context",
        "requires = [\"package-commands-v1\", \"package-context-v1\"]\n",
        "\n[context]\ndir = \"bundle\"\n",
        "#!/bin/sh\necho ok\n",
    );
    fs::create_dir(dir.join("bundle")).expect("create the bundle");
    fs::write(dir.join("bundle/note.md"), "note\n").expect("write a bundle file");
    let output = kit.kit(&["lint", "--dir", s(&dir)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("cannot be combined"),
        "the refusal names the combination: {}",
        stderr_of(&output)
    );
}

#[test]
fn kit_smoke_runs_every_command_of_a_commands_only_package() {
    let kit = Kit::new();
    let release = release_dir();
    let dir = write_commands_package(
        &kit,
        "commands-smoke",
        COMMANDS_REQUIRES,
        "",
        "#!/bin/sh\necho fleet-ok\n",
    );
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&dir),
        "--release-dir",
        s(&release),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "commands smoke failed: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["package_id"].as_str(), Some(COMMANDS_PACKAGE_ID));
    assert_eq!(document["commands"], serde_json::json!(["host-state"]));
    assert!(
        document.get("activity").is_none() && document.get("screen_lines").is_none(),
        "a commands smoke starts no session: {document:?}"
    );
    used_root(&document, &kit);
}

#[test]
fn kit_smoke_fails_when_a_command_exits_non_zero() {
    let kit = Kit::new();
    let release = release_dir();
    let dir = write_commands_package(
        &kit,
        "commands-smoke-failing",
        COMMANDS_REQUIRES,
        "",
        "#!/bin/sh\nexit 3\n",
    );
    let output = kit.kit(&["smoke", "--dir", s(&dir), "--release-dir", s(&release)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "rc is 2: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("host-state"),
        "the failure names the command: {}",
        stderr_of(&output)
    );
    named_root(&output);
}

#[test]
fn kit_smoke_refuses_runtime_only_inputs_for_a_commands_only_package() {
    let kit = Kit::new();
    let release = release_dir();
    let dir = write_commands_package(
        &kit,
        "commands-smoke-script",
        COMMANDS_REQUIRES,
        "",
        "#!/bin/sh\necho ok\n",
    );
    let script = kit.path("smoke-script.sh");
    fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write the script");
    make_executable(&script);
    for extra in [
        vec!["--script", s(&script)],
        vec!["--script-timeout", "120"],
        vec!["--detection-timeout", "30"],
    ] {
        let mut argv = vec![
            "smoke".to_owned(),
            "--dir".to_owned(),
            s(&dir).to_owned(),
            "--release-dir".to_owned(),
            s(&release).to_owned(),
        ];
        argv.extend(extra.iter().map(ToString::to_string));
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let output = kit.kit(&refs);
        assert_eq!(
            output.status.code(),
            Some(2),
            "rc is 2: {}",
            stderr_of(&output)
        );
        assert!(
            stderr_of(&output).contains("serves no runtime"),
            "the refusal names the runtime-only input: {}",
            stderr_of(&output)
        );
        if extra[0] == "--script-timeout" || extra[0] == "--detection-timeout" {
            assert!(
                stderr_of(&output).contains(extra[0]),
                "the commands-only refusal names the bound flag: {}",
                stderr_of(&output)
            );
        }
    }
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
    // The root exists before the upstream resolves, so the failure
    // names the used root and the root is already gone.
    named_root(&output);
}

#[test]
fn kit_smoke_failure_names_its_isolation_root() {
    let kit = Kit::new();
    // An empty release tree: the build passes, the root exists, then the
    // isolated host has no executables to copy.
    let empty = kit.dir("empty-release");
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&empty),
    ]);
    assert!(
        !output.status.success(),
        "an empty release fails the smoke: {}",
        stderr_of(&output)
    );
    named_root(&output);
}

#[test]
fn kit_scenarios_ignore_a_foreign_proof_root() {
    let kit = Kit::new();
    // A foreign temporary directory beside the real roots: no scenario
    // may trip over it, and the kit never cleans roots it did not use.
    let foreign = kit
        .root_parent
        .join(format!("{}foreign", plugin_kit::isolated::ROOT_PREFIX));
    fs::create_dir_all(&foreign).expect("the foreign root");
    fs::write(foreign.join("marker"), "foreign").expect("mark the foreign root");
    let release = release_dir();
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "smoke failed beside a foreign root: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let root = used_root(&document, &kit);
    assert_ne!(
        root.as_os_str(),
        foreign.as_os_str(),
        "the smoke used its own root"
    );
    assert_eq!(
        fs::read_to_string(foreign.join("marker")).expect("the marker survives"),
        "foreign",
        "the kit leaves foreign roots alone"
    );
    fs::remove_dir_all(&foreign).expect("remove the foreign root");
}

#[test]
fn kit_falls_back_to_the_short_parent_on_a_long_temp_root() {
    // A temp root too long for a socket path: the kit rejects it and
    // names a removed fallback root instead of failing the run.
    let kit = Kit::with_long_tmpdir(&"t".repeat(48));
    let release = release_dir();
    let output = kit.kit(&[
        "smoke",
        "--dir",
        s(&fixture_dir()),
        "--release-dir",
        s(&release),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "smoke failed on a long temp root: {}",
        stderr_of(&output)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let root = used_root(&document, &kit);
    assert!(
        !root.starts_with(&kit.root_parent),
        "the long temp root holds no kit root: {}",
        root.display()
    );
    assert!(
        root.starts_with(short_fallback()),
        "the fallback is the short parent: {}",
        root.display()
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
    named_root(&output);
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
    // throwaway root: the throwaway-signed index is foreign to it. The
    // root sits below the short temp root itself: below the deeper test
    // `tmp` directory its staged worker socket would exceed the limit.
    let workspace = pohunek_test_support::workspace_root();
    let root = Root::create_in(&kit.root_parent).expect("the isolation root");
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
}
