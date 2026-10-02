//! Fixture roots for tests that build trusted directories or Unix sockets.
//!
//! Pohunek's trusted filesystem layer refuses to traverse a symlinked path
//! component, and a Unix socket path must fit the platform's `sun_path`
//! (104 bytes on Darwin, 108 on Linux, both including the terminating NUL).
//! The standard temporary directory violates both on macOS: it is a per-user
//! directory under `/var/folders/...`, where `/var` is a symlink to
//! `/private/var`, and the prefix alone takes about 50 bytes. Tests that put
//! a trusted root or a socket under a fixture directory therefore take that
//! directory from this crate instead of from [`std::env::temp_dir`].
//!
//! [`temp_root`] names the base directory; [`tempdir`] and [`tempdir_with_prefix`]
//! create a private, uniquely named directory under it that is removed when
//! the returned guard drops.
//!
//! It also resolves the runtime artifacts tests depend on: [`manifest_dir`],
//! [`workspace_root`], [`bin_exe`] and [`worker_binary`] read the test
//! process's environment or its own executable location when called, never
//! a path baked in at compile time. A nextest archive extracted at a different
//! absolute path therefore finds its binaries and source files.
//!
//! The [`mod@env`] module holds [`env::TestEnv`], the per-test fixture that owns a
//! private root, working directory, `HOME` and XDG directories, and builds a
//! scrubbed environment for child processes.
//!
//! The [`wait`] module holds the readiness waits: [`wait::HANG_GUARD`], the
//! single ceiling every incidental-deadline wait is bounded by, and
//! [`wait::poll_until`], [`wait::wait_until`] and [`wait::guard`], which fail
//! with a message naming the awaited condition instead of hanging until nextest
//! terminates the test.
//!
//! The [`time`] module holds [`time::AutoAdvanceInhibitor`] and
//! [`time::TIMER_TICK`], which let a test on tokio's paused clock await real
//! I/O without the clock racing ahead of it.
//!
//! This crate is a development dependency only; production code never picks
//! its paths from here.
//!
//! # Examples
//!
//! ```
//! let fixture = pohunek_test_support::tempdir()?;
//! assert!(fixture.path().starts_with(pohunek_test_support::temp_root()));
//! # Ok::<(), std::io::Error>(())
//! ```

// Rust guideline compliant 2026-10-02

pub mod env;
pub mod time;
pub mod wait;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// Resolved, short system temporary directory on macOS.
///
/// `/tmp` is a symlink to this directory on macOS. pohunek's own macOS
/// runtime default (`/private/tmp/pohunek-<uid>`) uses the same base, so
/// fixtures under it face the same trusted-filesystem checks as production.
#[cfg(target_os = "macos")]
const MACOS_TEMP_ROOT: &str = "/private/tmp";

/// Name prefix of the directories [`tempdir`] creates.
///
/// Kept short because every byte of a fixture root counts against the
/// `sun_path` limit of the sockets tests bind beneath it.
const DEFAULT_PREFIX: &str = "ph-";

/// Environment variable Cargo sets to the package directory of the running test.
const MANIFEST_DIR_VAR: &str = "CARGO_MANIFEST_DIR";

/// Prefix of the per-binary variables Cargo sets for integration tests.
const BIN_EXE_PREFIX: &str = "CARGO_BIN_EXE_";

/// Environment variable that points tests at an explicit session worker binary.
const WORKER_OVERRIDE_VAR: &str = "POHUNEK_WORKER_BIN";

/// File name of the session worker inside the Cargo profile directory.
const WORKER_FILE_NAME: &str = "pohunek-sessiond";

/// Name of the directory Cargo places test binaries in, below the profile directory.
const DEPS_DIR_NAME: &str = "deps";

/// Command that builds the session worker, quoted in the missing-worker panic.
const WORKER_BUILD_HINT: &str = "cargo build -p pohunek-session-worker --bin pohunek-sessiond";

/// Mode of every fixture directory.
///
/// The trusted filesystem layer rejects group- or world-accessible private
/// roots, so fixtures start owner-only like production state directories.
pub(crate) const PRIVATE_DIR_MODE: u32 = 0o700;

/// Sets `path` to [`PRIVATE_DIR_MODE`] exactly.
///
/// The mode passed when a directory is created is filtered by the process
/// umask, which only removes bits: a mask such as `0o077` leaves `0o700`, but
/// one that clears owner bits leaves a directory its owner cannot enter. An
/// explicit `chmod` after creation makes the mode independent of the umask.
pub(crate) fn make_private(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

/// Returns the base directory test fixtures create their roots under.
///
/// On macOS this is `/private/tmp`: symlink-free and short enough for socket
/// paths nested several levels below it. Elsewhere it is
/// [`std::env::temp_dir`], which already satisfies both constraints.
#[must_use]
pub fn temp_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(MACOS_TEMP_ROOT)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::temp_dir()
    }
}

/// Creates a private fixture directory under [`temp_root`].
///
/// This is the drop-in replacement for [`tempfile::tempdir`]. The directory is
/// owner-only (`0700`, whatever the process umask) and is removed with its contents when the returned
/// guard drops.
///
/// # Errors
///
/// Returns the I/O error when the directory cannot be created.
pub fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempdir_with_prefix(DEFAULT_PREFIX)
}

/// Creates a private fixture directory under [`temp_root`] named `prefix` plus
/// a random suffix.
///
/// Use a short `prefix`: it counts against the socket path limit of anything
/// bound beneath the directory.
///
/// # Errors
///
/// Returns the I/O error when the directory cannot be created.
pub fn tempdir_with_prefix(prefix: &str) -> std::io::Result<tempfile::TempDir> {
    // The mode requested at creation keeps the directory owner-only from the
    // first instant; the explicit chmod makes it exactly 0700 under any umask.
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
        .tempdir_in(temp_root())?;
    make_private(dir.path())?;
    Ok(dir)
}

/// Returns the value of a required variable or a message naming it.
fn require_var(name: &str, value: Option<OsString>) -> Result<OsString, String> {
    value.ok_or_else(|| format!("environment variable {name} is not set; run the test through `cargo test` or `cargo nextest run`"))
}

/// Derives the workspace root from a package directory of the `crates/<name>` layout.
fn workspace_root_of(manifest_dir: &Path) -> Option<PathBuf> {
    manifest_dir.parent()?.parent().map(Path::to_path_buf)
}

/// Derives the Cargo profile directory from the path of a test executable.
///
/// Test binaries live at `<profile>/deps/<name>-<hash>`, where `<profile>` is
/// `target/debug` or `target/<triple>/debug`.
fn profile_dir_of(exe: &Path) -> Result<PathBuf, String> {
    let deps = exe
        .parent()
        .filter(|dir| dir.file_name().is_some_and(|name| name == DEPS_DIR_NAME));
    deps.and_then(Path::parent).map(Path::to_path_buf).ok_or_else(|| {
        format!(
            "test executable {} is not in a `{DEPS_DIR_NAME}` directory of a Cargo profile; set {WORKER_OVERRIDE_VAR} to the worker binary",
            exe.display()
        )
    })
}

/// Resolves the worker path: the override when present (unchanged), else the
/// worker file inside the profile directory of `exe`.
fn resolve_worker(override_path: Option<OsString>, exe: &Path) -> Result<PathBuf, String> {
    match override_path {
        Some(path) => Ok(PathBuf::from(path)),
        None => profile_dir_of(exe).map(|dir| dir.join(WORKER_FILE_NAME)),
    }
}

/// Returns the package directory of the crate whose test is running.
///
/// Reads `CARGO_MANIFEST_DIR` from the process environment when called, so the
/// result follows an extracted nextest archive instead of the build location.
///
/// # Panics
///
/// Panics when `CARGO_MANIFEST_DIR` is not set, which means the test was not
/// started through Cargo or nextest.
///
/// # Examples
///
/// ```
/// assert!(pohunek_test_support::manifest_dir().join("Cargo.toml").is_file());
/// ```
#[must_use]
pub fn manifest_dir() -> PathBuf {
    match require_var(MANIFEST_DIR_VAR, std::env::var_os(MANIFEST_DIR_VAR)) {
        Ok(dir) => PathBuf::from(dir),
        Err(message) => panic!("{message}"),
    }
}

/// Returns the workspace root, two levels above the package directory.
///
/// Relies on the `crates/<name>` layout of this workspace.
///
/// # Panics
///
/// Panics when [`manifest_dir`] panics, or when the derived directory holds no
/// `Cargo.toml`.
///
/// # Examples
///
/// ```
/// assert!(pohunek_test_support::workspace_root().join("Cargo.toml").is_file());
/// ```
#[must_use]
pub fn workspace_root() -> PathBuf {
    let manifest = manifest_dir();
    let root = workspace_root_of(&manifest).unwrap_or_else(|| {
        panic!(
            "package directory {} has no workspace root two levels up",
            manifest.display()
        )
    });
    assert!(
        root.join("Cargo.toml").is_file(),
        "{} is not a workspace root (no Cargo.toml); expected the crates/<name> layout",
        root.display()
    );
    root
}

/// Returns the path of a binary of the running test's own package.
///
/// Reads `CARGO_BIN_EXE_<name>` when called; Cargo sets it for integration
/// tests, with `name` spelled as in the `[[bin]]` target (hyphens included).
///
/// # Panics
///
/// Panics naming the variable when it is not set, for example in a unit test
/// inside `src/` or for a binary of another package.
#[must_use]
pub fn bin_exe(name: &str) -> PathBuf {
    let var = format!("{BIN_EXE_PREFIX}{name}");
    match require_var(&var, std::env::var_os(&var)) {
        Ok(path) => PathBuf::from(path),
        Err(message) => panic!("{message}"),
    }
}

/// Returns the path of the `pohunek-sessiond` session worker binary.
///
/// `POHUNEK_WORKER_BIN`, when set, is returned unchanged. Otherwise the worker
/// is expected beside the test's Cargo profile directory, derived from the
/// running test executable, so it works in an extracted nextest archive.
///
/// # Panics
///
/// Panics when the test executable is not under a profile `deps` directory, or
/// when the derived path is not a file; the message names
/// `cargo build -p pohunek-session-worker --bin pohunek-sessiond`.
#[must_use]
pub fn worker_binary() -> PathBuf {
    let override_path = std::env::var_os(WORKER_OVERRIDE_VAR);
    let overridden = override_path.is_some();
    let exe = std::env::current_exe()
        .unwrap_or_else(|error| panic!("cannot determine the test executable path: {error}"));
    let path = resolve_worker(override_path, &exe).unwrap_or_else(|message| panic!("{message}"));
    assert!(
        overridden || path.is_file(),
        "session worker {} does not exist; build it with `{WORKER_BUILD_HINT}`",
        path.display()
    );
    path
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{
        profile_dir_of, require_var, resolve_worker, temp_root, tempdir, tempdir_with_prefix,
        workspace_root_of,
    };

    #[test]
    fn override_is_returned_unchanged_without_a_file_check() {
        let path = resolve_worker(
            Some(OsString::from("/nonexistent/relative/../worker")),
            Path::new("/irrelevant/not-in-deps"),
        );
        assert_eq!(path, Ok(PathBuf::from("/nonexistent/relative/../worker")));
    }

    #[test]
    fn profile_dir_comes_from_a_host_build() {
        let exe = Path::new("/x/target/debug/deps/state_authority-0123abcd");
        assert_eq!(profile_dir_of(exe), Ok(PathBuf::from("/x/target/debug")));
        assert_eq!(
            resolve_worker(None, exe),
            Ok(PathBuf::from("/x/target/debug/pohunek-sessiond"))
        );
    }

    #[test]
    fn profile_dir_comes_from_a_cross_build() {
        let exe = Path::new("/x/target/aarch64-apple-darwin/debug/deps/cli-0123abcd");
        assert_eq!(
            profile_dir_of(exe),
            Ok(PathBuf::from("/x/target/aarch64-apple-darwin/debug"))
        );
    }

    #[test]
    fn executable_outside_deps_is_an_error_naming_the_override() {
        let error = profile_dir_of(Path::new("/x/target/debug/cli")).unwrap_err();
        assert!(error.contains("POHUNEK_WORKER_BIN"), "{error}");
        resolve_worker(None, Path::new("/x/target/debug/cli")).unwrap_err();
    }

    #[test]
    fn missing_variable_error_names_the_variable() {
        let error = require_var("CARGO_BIN_EXE_some-bin", None).unwrap_err();
        assert!(error.contains("CARGO_BIN_EXE_some-bin"), "{error}");
        assert_eq!(
            require_var("V", Some(OsString::from("/p"))),
            Ok(OsString::from("/p"))
        );
    }

    #[test]
    fn workspace_root_is_two_levels_above_the_package() {
        assert_eq!(
            workspace_root_of(Path::new("/w/crates/daemon")),
            Some(PathBuf::from("/w"))
        );
        assert_eq!(workspace_root_of(Path::new("/w")), None);
    }

    #[test]
    fn workspace_root_holds_the_workspace_manifest() {
        let root = super::workspace_root();
        assert!(root.join("Cargo.toml").is_file(), "{}", root.display());
        assert!(root.join("crates/test-support").is_dir());
    }

    /// Darwin's `sun_path` capacity, including the terminating NUL.
    #[cfg(target_os = "macos")]
    const DARWIN_SUN_PATH_CAPACITY: usize = 104;

    /// Longest suffix the daemon tests append below a fixture root for a
    /// socket, e.g. `/runtime/pohunek/workers/<session-id>/control.sock`.
    #[cfg(target_os = "macos")]
    const NESTED_SOCKET_SUFFIX: usize = 64;

    #[test]
    fn temp_root_has_no_symlinked_component() {
        let root = temp_root();
        let canonical = std::fs::canonicalize(&root).expect("canonicalize temp root");
        assert_eq!(root, canonical);
    }

    #[test]
    fn fixture_is_private_and_removed_on_drop() {
        let fixture = tempdir().expect("create fixture");
        let path = fixture.path().to_path_buf();
        assert!(path.starts_with(temp_root()));
        let mode = std::fs::metadata(&path)
            .expect("stat fixture")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, super::PRIVATE_DIR_MODE);
        drop(fixture);
        assert!(!path.exists());
    }

    #[test]
    fn fixture_uses_the_requested_prefix() {
        let fixture = tempdir_with_prefix("ph-prefix-").expect("create fixture");
        let name = fixture
            .path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("utf-8 fixture name");
        assert!(name.starts_with("ph-prefix-"), "{name}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_fixture_leaves_room_for_nested_sockets() {
        let fixture = tempdir().expect("create fixture");
        let length = fixture.path().as_os_str().len();
        assert!(
            length + NESTED_SOCKET_SUFFIX < DARWIN_SUN_PATH_CAPACITY,
            "{length}"
        );
    }

    /// Prefix of the lines the umask probe child prints.
    const UMASK_PROBE_LINE: &str = "umask-probe:";

    /// Umask that removes every permission bit, the worst case for `mkdir`.
    const OWNER_MASKING_UMASK: &str = "777";

    /// Re-executes this test binary for `probe` under an owner-masking umask and
    /// returns the probe's `umask-probe:` lines.
    ///
    /// The umask is process-global, so it is set in a child: POSIX `sh` has the
    /// `umask` builtin and exists at `/bin/sh` on Linux and macOS.
    fn run_probe_under_masking_umask(probe: &str) -> Vec<String> {
        let exe = std::env::current_exe().expect("test executable");
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("umask {OWNER_MASKING_UMASK}; exec \"$0\" \"$@\""),
            ])
            .arg(exe)
            .args(["--ignored", "--exact", probe, "--nocapture"])
            .output()
            .expect("run probe child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "probe {probe} failed: {stderr}");
        String::from_utf8(output.stdout)
            .expect("utf-8 stdout")
            .lines()
            .filter_map(|line| line.strip_prefix(UMASK_PROBE_LINE))
            .map(str::to_owned)
            .collect()
    }

    /// Child half of [`fixture_is_private_under_an_owner_masking_umask`].
    #[test]
    #[ignore = "child process of fixture_is_private_under_an_owner_masking_umask"]
    fn umask_probe_tempdir() {
        let fixture = tempdir().expect("create fixture under the masking umask");
        std::fs::write(fixture.path().join("file"), b"x").expect("write into the fixture");
        let mode = std::fs::metadata(fixture.path())
            .expect("stat fixture")
            .permissions()
            .mode();
        println!("{UMASK_PROBE_LINE}{:o}", mode & 0o777);
    }

    #[test]
    fn fixture_is_private_under_an_owner_masking_umask() {
        let lines = run_probe_under_masking_umask("tests::umask_probe_tempdir");
        assert_eq!(lines, [format!("{:o}", super::PRIVATE_DIR_MODE)]);
    }
}
