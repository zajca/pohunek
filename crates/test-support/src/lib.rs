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
//! The [`process_env`] module holds [`process_env::ProcessEnv`], the one
//! binary-wide lock and unwind-safe override for tests that must change the
//! process environment in-process.
//!
//! The [`time`] module holds [`time::AutoAdvanceInhibitor`] and
//! [`time::TIMER_TICK`], which let a test on tokio's paused clock await real
//! I/O without the clock racing ahead of it.
//!
//! The [`mod@fs`] module holds [`fs::write_executable`] and [`fs::write_file`],
//! which write fixture files through a child process so that no write
//! descriptor in this process can make a later `exec` fail with `ETXTBSY`.
//!
//! The [`workers`] module holds [`workers::WorkerGuard`], which terminates the
//! `pohunek-sessiond` workers a fixture root owns when it drops and fails the
//! test that left them running; [`env::TestEnv`] carries one for its root.
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
pub mod fs;
pub mod process_env;
pub mod time;
pub mod wait;
pub mod workers;

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

/// Resolves `raw` to an absolute path without symlinked components.
///
/// A relative `raw` is taken relative to the directory `cwd` returns; an
/// absolute one is canonicalized directly and `cwd` is never called, so an
/// unreadable working directory only matters when it is needed. Taking the
/// working directory as a provider keeps callers and tests independent of
/// process-global state.
///
/// # Errors
///
/// Returns the I/O error when `cwd` fails for a relative `raw`, or when the
/// resolved path does not exist or cannot be canonicalized.
fn resolve_root(
    raw: &Path,
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
) -> std::io::Result<PathBuf> {
    if raw.is_absolute() {
        std::fs::canonicalize(raw)
    } else {
        std::fs::canonicalize(cwd()?.join(raw))
    }
}

/// Returns the base directory test fixtures create their roots under.
///
/// The result is always absolute and free of symlinked components, whatever
/// `TMPDIR` holds: a relative `TMPDIR` is resolved against the working
/// directory and a symlinked one is replaced by its target. On macOS the base
/// is `/private/tmp`, short enough for socket paths nested several levels
/// below it. Elsewhere it is [`std::env::temp_dir`].
///
/// # Panics
///
/// Panics when the base directory does not exist or cannot be resolved, or when
/// it is relative and the working directory cannot be determined, since no
/// fixture can be created beneath it.
#[must_use]
pub fn temp_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    let raw = PathBuf::from(MACOS_TEMP_ROOT);
    #[cfg(not(target_os = "macos"))]
    let raw = std::env::temp_dir();
    resolve_root(&raw, std::env::current_dir).unwrap_or_else(|error| {
        panic!(
            "temporary root {} cannot be resolved to a canonical path: {error}",
            raw.display()
        )
    })
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

    use super::{profile_dir_of, resolve_root, resolve_worker, temp_root, tempdir};

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

    /// Working-directory provider that fails the test when it is consulted.
    fn never_called_cwd() -> std::io::Result<PathBuf> {
        panic!("the working directory must not be read for an absolute root")
    }

    #[test]
    fn resolve_root_makes_a_relative_root_absolute_and_canonical() {
        let fixture = tempdir().expect("create fixture");
        let base = std::fs::canonicalize(fixture.path()).expect("canonicalize fixture");
        std::fs::create_dir_all(base.join("a/b")).expect("create nested directory");
        let resolved = resolve_root(Path::new("b/../b"), || Ok(base.join("a"))).expect("resolve");
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert_eq!(resolved, base.join("a/b"));
    }

    #[test]
    fn resolve_root_follows_a_symlinked_root_to_the_real_path() {
        let fixture = tempdir().expect("create fixture");
        let base = std::fs::canonicalize(fixture.path()).expect("canonicalize fixture");
        let real = base.join("real");
        std::fs::create_dir(&real).expect("create real directory");
        std::os::unix::fs::symlink(&real, base.join("link")).expect("create symlink");
        let resolved =
            resolve_root(&base.join("link"), never_called_cwd).expect("resolve absolute link");
        assert_eq!(resolved, real);
        let relative =
            resolve_root(Path::new("link"), || Ok(base.clone())).expect("resolve relative link");
        assert_eq!(relative, real);
    }

    #[test]
    fn resolve_root_resolves_an_absolute_root_when_the_cwd_is_unreadable() {
        let fixture = tempdir().expect("create fixture");
        let base = std::fs::canonicalize(fixture.path()).expect("canonicalize fixture");
        let resolved = resolve_root(&base, never_called_cwd).expect("resolve");
        assert_eq!(resolved, base);
    }

    #[test]
    fn resolve_root_propagates_a_cwd_failure_for_a_relative_root() {
        let error = resolve_root(Path::new("relative"), || {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn resolve_root_reports_a_missing_root() {
        let fixture = tempdir().expect("create fixture");
        let error =
            resolve_root(Path::new("missing"), || Ok(fixture.path().to_path_buf())).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
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
