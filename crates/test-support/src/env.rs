//! A hermetic per-test environment: private directories and an allowlisted
//! process environment.
//!
//! A test that spawns processes or builds trusted directories must not depend
//! on, or write into, state that belongs to the host or to a sibling test.
//! [`TestEnv`] owns, for one test:
//!
//! - a short private root under [`crate::temp_root`], removed when the
//!   [`TestEnv`] drops (also while a panic unwinds). The base must be
//!   symlink-free and short enough for nested sockets; otherwise creation
//!   fails with a message naming `TMPDIR`;
//! - a private working directory, `HOME`, the five XDG base directories and a
//!   `TMPDIR`, all inside that root and owner-only (`0700`, whatever the process
//!   umask);
//! - an environment for child processes built from an allowlist: only the
//!   parent variables named by [`INHERITED_VARS`] are passed on, `HOME`,
//!   `XDG_*` and `TMPDIR` point at the private directories, and every other
//!   variable is dropped.
//!
//! An allowlist is used because ambient variables steer children towards host
//! state in ways no denylist anticipates: shell startup hooks (`BASH_ENV`,
//! `ENV`, `ZDOTDIR`), interpreter paths (`PYTHONPATH`, `PYTHONSTARTUP`),
//! dynamic-linker injection (`LD_PRELOAD`), and credentials (`GH_TOKEN`,
//! `*_API_KEY`).
//!
//! The scrub applies to children only. Library code that reads the process
//! environment in-process, such as `std::env::var_os("POHUNEK_SESSION_ID")`,
//! still sees the test process's own variables; a `TestEnv` cannot change that.
//!
//! Children are started through [`TestEnv::command`] or
//! [`TestEnv::tokio_command`]. Both clear the inherited environment, so a
//! variable a child needs and the allowlist does not cover, such as
//! `POHUNEK_WORKER_BIN`, `SHELL` or the cargo and rustup variables, is added
//! explicitly on the returned
//! command; variables added there are passed on unchanged.
//!
//! Unix socket paths below the root are checked against
//! [`STRICTEST_SOCKET_PATH_CAPACITY`] by [`TestEnv::socket_path`].
//!
//! # Examples
//!
//! ```
//! use pohunek_test_support::env::TestEnv;
//!
//! let env = TestEnv::new()?;
//! assert!(env.home().starts_with(env.root()));
//! let socket = env.socket_path("run/pohunek/control.sock")?;
//! assert!(socket.starts_with(env.root()));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

use crate::{make_private, PRIVATE_DIR_MODE};

/// Capacity of a Unix socket path on the strictest supported platform, in bytes.
///
/// Darwin's `sun_path` holds 104 bytes including the terminating NUL (Linux
/// holds 108). A path that fits this limit binds on every supported platform;
/// raising the value lets tests pass on Linux and fail on macOS.
pub const STRICTEST_SOCKET_PATH_CAPACITY: usize = 104;

/// Name prefix of the [`TestEnv`] root.
///
/// Short because every byte of the root counts against
/// [`STRICTEST_SOCKET_PATH_CAPACITY`] for each socket bound beneath it.
const ROOT_PREFIX: &str = "ph-";

/// Number of random characters in the root name.
///
/// Fixed so the root name length, and with it the base length budget, is known
/// before the root is created.
const ROOT_RANDOM_BYTES: usize = 6;

/// The longest socket path the daemon and worker tests bind below a root,
/// relative to it: the runtime directory, the pohunek runtime subdirectory,
/// a 36-character worker id and the control socket name.
///
/// [`TestEnv::with_base`] requires `<base>/<root name>` plus this suffix to fit
/// [`STRICTEST_SOCKET_PATH_CAPACITY`].
const NESTED_SOCKET_SUFFIX: &str =
    "/run/pohunek/workers/0123456789abcdef0123456789abcdef0123/control.sock";

/// Directory names inside the root.
///
/// Kept to a few bytes each for the same socket-path budget as [`ROOT_PREFIX`].
const HOME_DIR: &str = "home";
const CWD_DIR: &str = "work";
const CONFIG_DIR: &str = "config";
const DATA_DIR: &str = "data";
const STATE_DIR: &str = "state";
const CACHE_DIR: &str = "cache";
const RUNTIME_DIR: &str = "run";
const TMP_DIR: &str = "tmp";

/// Environment variable names pinned to private directories.
const HOME_VAR: &str = "HOME";
const XDG_CONFIG_HOME_VAR: &str = "XDG_CONFIG_HOME";
const XDG_DATA_HOME_VAR: &str = "XDG_DATA_HOME";
const XDG_STATE_HOME_VAR: &str = "XDG_STATE_HOME";
const XDG_CACHE_HOME_VAR: &str = "XDG_CACHE_HOME";
const XDG_RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";
const TMPDIR_VAR: &str = "TMPDIR";

/// Exact variable names passed from the parent to every child environment.
///
/// Everything else is dropped, including `POHUNEK_*` (an enclosing session's
/// identity makes a child fail origin validation), `XDG_*` and `HOME` (replaced
/// by private directories), `GIT_*`, `SSH_AUTH_SOCK`, `DBUS_*`, shell startup
/// hooks, interpreter search paths, dynamic-linker variables and credentials.
/// A test that needs one of them adds it on the returned command.
///
/// Each entry has a reason to cross the boundary:
///
/// - `PATH`: children locate tools through it. Pinning the tools themselves is a
///   separate concern of the test that needs a specific tool.
/// - `LANG`, `LANGUAGE` and the POSIX and glibc locale categories (`LC_ALL`,
///   `LC_CTYPE`, `LC_COLLATE`, `LC_MESSAGES`, `LC_MONETARY`, `LC_NUMERIC`,
///   `LC_TIME`, `LC_ADDRESS`, `LC_IDENTIFICATION`, `LC_MEASUREMENT`, `LC_NAME`,
///   `LC_PAPER`, `LC_TELEPHONE`): they select the character encoding and
///   message language of child tools and carry no paths or credentials. They
///   are listed one by one because a `LC_` prefix would also admit names such as
///   `LC_API_KEY`.
/// - `RUST_BACKTRACE`: diagnostics only; a panicking child reports where.
///
/// Deliberately absent:
///
/// - The cargo and rustup variables (`CARGO_HOME`, `RUSTUP_HOME`,
///   `RUSTUP_TOOLCHAIN`, `CARGO_TARGET_DIR`, `RUSTFLAGS`, `RUSTDOCFLAGS`,
///   `CARGO_ENCODED_RUSTFLAGS`, `CARGO_*`). `CARGO_HOME` holds
///   `credentials.toml` and the shared registry cache, and `CARGO_TARGET_DIR`
///   is a shared writable build directory, so passing them would give every
///   child the developer's registry credentials and write access to shared
///   build state. A test that must run `cargo` or `rustup` passes exactly the
///   variables it needs on the returned command.
/// - `USER`/`LOGNAME` (the account name is the developer's, nothing needs it,
///   and absence is valid), `TERM` and `TZ` (host display and clock
///   configuration; a test that depends on them sets them), `SHELL`,
///   `RUST_LOG` (changes child log output), `RUSTC_WRAPPER` (runs an arbitrary
///   binary around the compiler), and `LD_*` and `DYLD_*` (can inject
///   libraries into a child).
pub const INHERITED_VARS: &[&str] = &[
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_ADDRESS",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_NAME",
    "LC_PAPER",
    "LC_TELEPHONE",
    "RUST_BACKTRACE",
];

/// Why a socket path is not usable under a [`TestEnv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketPathError {
    /// The path, with its terminating NUL, exceeds [`STRICTEST_SOCKET_PATH_CAPACITY`].
    TooLong {
        /// The rejected path.
        path: PathBuf,
        /// Length of `path` in bytes, excluding the terminating NUL.
        len: usize,
    },
    /// The requested path is absolute or contains `..`, so it would leave the root.
    EscapesRoot {
        /// The rejected relative path.
        path: PathBuf,
    },
}

impl fmt::Display for SocketPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { path, len } => write!(
                f,
                "socket path {} is {len} bytes; with its NUL it must fit {STRICTEST_SOCKET_PATH_CAPACITY} bytes",
                path.display()
            ),
            Self::EscapesRoot { path } => write!(
                f,
                "socket path {} must be relative to the test root and contain no `..`",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SocketPathError {}

/// Checks that `path` fits the strictest supported `sun_path`.
///
/// # Errors
///
/// Returns [`SocketPathError::TooLong`] when the path plus its terminating
/// NUL exceeds [`STRICTEST_SOCKET_PATH_CAPACITY`] bytes.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use pohunek_test_support::env::check_socket_path;
///
/// check_socket_path(Path::new("/tmp/ph-abc123/run/control.sock"))?;
/// check_socket_path(Path::new(&"/a".repeat(60))).unwrap_err();
/// # Ok::<(), pohunek_test_support::env::SocketPathError>(())
/// ```
pub fn check_socket_path(path: &Path) -> Result<(), SocketPathError> {
    let len = path.as_os_str().as_bytes().len();
    if len < STRICTEST_SOCKET_PATH_CAPACITY {
        Ok(())
    } else {
        Err(SocketPathError::TooLong {
            path: path.to_path_buf(),
            len,
        })
    }
}

/// A private root, working directory, HOME and XDG directories, and a
/// scrubbed child environment, owned by one test.
///
/// Dropping the value removes the root and everything beneath it, on success
/// and while a panic unwinds. A test process that is killed or aborts cannot
/// run the drop and leaves its root behind.
///
/// The `Debug` output lists the directories and the names of the child
/// environment variables, never their values: the parent environment can hold
/// credentials such as `GH_TOKEN` that an allowlisted variable might carry.
pub struct TestEnv {
    root: TempDir,
    cwd: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    state_home: PathBuf,
    cache_home: PathBuf,
    runtime_dir: PathBuf,
    tmp_dir: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}

impl fmt::Debug for TestEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestEnv")
            .field("root", &self.root.path())
            .field("cwd", &self.cwd)
            .field("home", &self.home)
            .field("config_home", &self.config_home)
            .field("data_home", &self.data_home)
            .field("state_home", &self.state_home)
            .field("cache_home", &self.cache_home)
            .field("runtime_dir", &self.runtime_dir)
            .field("tmp_dir", &self.tmp_dir)
            .field(
                "environment_names",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl TestEnv {
    /// Creates a private environment derived from the current process environment.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::ErrorKind::InvalidInput`] error naming `TMPDIR`
    /// when the base directory is not canonical or too long for nested
    /// sockets, and the I/O error when the root or one of its directories
    /// cannot be created.
    pub fn new() -> std::io::Result<Self> {
        Self::with_parent_environment(std::env::vars_os())
    }

    /// Creates a private environment derived from an explicit parent environment.
    ///
    /// [`TestEnv::new`] passes the current process environment; tests of the
    /// scrub pass a crafted one instead of mutating their own.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::ErrorKind::InvalidInput`] error naming `TMPDIR`
    /// when the base directory is not canonical or too long for nested
    /// sockets, and the I/O error when the root or one of its directories
    /// cannot be created.
    pub fn with_parent_environment(
        parent: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> std::io::Result<Self> {
        Self::with_base(&crate::temp_root(), parent)
    }

    /// Creates the environment with its root below `base`.
    ///
    /// `base` must be canonical, so no component is a symlink, and short enough
    /// that `<base>/<root name>` plus [`NESTED_SOCKET_SUFFIX`] fits
    /// [`STRICTEST_SOCKET_PATH_CAPACITY`]. Nothing is created when it is not.
    fn with_base(
        base: &Path,
        parent: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> std::io::Result<Self> {
        validate_base(base)?;
        let root = tempfile::Builder::new()
            .prefix(ROOT_PREFIX)
            .rand_bytes(ROOT_RANDOM_BYTES)
            .permissions(std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
            .tempdir_in(base)?;
        // Before any child is created: a umask that clears owner bits would
        // otherwise leave a root the owner cannot enter.
        make_private(root.path())?;
        let child = |name: &str| root.path().join(name);
        let (cwd, home) = (child(CWD_DIR), child(HOME_DIR));
        let (config_home, data_home) = (child(CONFIG_DIR), child(DATA_DIR));
        let (state_home, cache_home) = (child(STATE_DIR), child(CACHE_DIR));
        let (runtime_dir, tmp_dir) = (child(RUNTIME_DIR), child(TMP_DIR));
        for dir in [
            &cwd,
            &home,
            &config_home,
            &data_home,
            &state_home,
            &cache_home,
            &runtime_dir,
            &tmp_dir,
        ] {
            std::fs::DirBuilder::new()
                .mode(PRIVATE_DIR_MODE)
                .create(dir)?;
            make_private(dir)?;
        }
        let pinned = [
            (HOME_VAR, &home),
            (XDG_CONFIG_HOME_VAR, &config_home),
            (XDG_DATA_HOME_VAR, &data_home),
            (XDG_STATE_HOME_VAR, &state_home),
            (XDG_CACHE_HOME_VAR, &cache_home),
            (XDG_RUNTIME_DIR_VAR, &runtime_dir),
            (TMPDIR_VAR, &tmp_dir),
        ]
        .map(|(name, dir)| (OsString::from(name), dir.clone().into_os_string()));
        let environment = scrubbed(parent, pinned);
        Ok(Self {
            root,
            cwd,
            home,
            config_home,
            data_home,
            state_home,
            cache_home,
            runtime_dir,
            tmp_dir,
            environment,
        })
    }

    /// Returns the private root; every other directory is beneath it.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Returns the working directory of commands started by this environment.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Returns the private `HOME`.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Returns the private `XDG_CONFIG_HOME`.
    #[must_use]
    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    /// Returns the private `XDG_DATA_HOME`.
    #[must_use]
    pub fn data_home(&self) -> &Path {
        &self.data_home
    }

    /// Returns the private `XDG_STATE_HOME`.
    #[must_use]
    pub fn state_home(&self) -> &Path {
        &self.state_home
    }

    /// Returns the private `XDG_CACHE_HOME`.
    #[must_use]
    pub fn cache_home(&self) -> &Path {
        &self.cache_home
    }

    /// Returns the private `XDG_RUNTIME_DIR` (mode `0700`).
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Returns the private `TMPDIR`.
    #[must_use]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// Returns the environment every child starts with.
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    /// Returns a command for `program` with a cleared, scrubbed environment and
    /// the private working directory.
    ///
    /// Variables added to the returned command are passed on unchanged.
    #[must_use]
    pub fn command(&self, program: impl AsRef<OsStr>) -> std::process::Command {
        let mut command = std::process::Command::new(program);
        command
            .env_clear()
            .envs(&self.environment)
            .current_dir(&self.cwd);
        command
    }

    /// Returns a [`tokio::process::Command`] configured like [`TestEnv::command`].
    #[must_use]
    pub fn tokio_command(&self, program: impl AsRef<OsStr>) -> tokio::process::Command {
        tokio::process::Command::from(self.command(program))
    }

    /// Returns `relative` joined below the root after checking that a socket
    /// bound there fits [`STRICTEST_SOCKET_PATH_CAPACITY`].
    ///
    /// A typed error instead of a panic lets a test that probes the limit
    /// assert the failure, while every other caller turns it into a failure
    /// naming the path and its length with `?` or `expect`.
    ///
    /// # Errors
    ///
    /// Returns [`SocketPathError::EscapesRoot`] when `relative` is absolute or
    /// contains `..`, and [`SocketPathError::TooLong`] when the joined path
    /// does not fit.
    pub fn socket_path(&self, relative: impl AsRef<Path>) -> Result<PathBuf, SocketPathError> {
        let relative = relative.as_ref();
        let stays_below = relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        if !stays_below {
            return Err(SocketPathError::EscapesRoot {
                path: relative.to_path_buf(),
            });
        }
        let path = self.root().join(relative);
        check_socket_path(&path)?;
        Ok(path)
    }
}

/// Checks that `base` is canonical and leaves room for nested sockets.
///
/// On Linux the base follows the ambient `TMPDIR`, so a long or symlinked
/// `TMPDIR` would otherwise break the short, trusted root silently.
fn validate_base(base: &Path) -> std::io::Result<()> {
    let invalid = |detail: String| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "test root base {} (from TMPDIR on Linux) {detail}",
                base.display()
            ),
        )
    };
    let canonical = std::fs::canonicalize(base)
        .map_err(|error| invalid(format!("cannot be resolved: {error}")))?;
    if canonical != base {
        return Err(invalid(format!(
            "is not canonical (it resolves to {}); a symlinked component breaks the trusted filesystem checks, so set TMPDIR to a symlink-free directory",
            canonical.display()
        )));
    }
    let longest = base.as_os_str().len()
        + 1
        + ROOT_PREFIX.len()
        + ROOT_RANDOM_BYTES
        + NESTED_SOCKET_SUFFIX.len();
    if longest >= STRICTEST_SOCKET_PATH_CAPACITY {
        return Err(invalid(format!(
            "is too long: a nested socket path would be {longest} bytes, and with its NUL it must fit {STRICTEST_SOCKET_PATH_CAPACITY} bytes; set TMPDIR to a shorter directory"
        )));
    }
    Ok(())
}

/// Returns whether `name` is passed from the parent to a child environment.
fn is_inherited(name: &OsStr) -> bool {
    INHERITED_VARS
        .iter()
        .any(|var| name.as_bytes() == var.as_bytes())
}

/// Builds a child environment: the allowlisted variables of `parent`, plus `pinned`.
fn scrubbed(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    pinned: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    parent
        .into_iter()
        .filter(|(name, _)| is_inherited(name))
        .chain(pinned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt as _;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    /// Marker the probe child test reacts to; it is added after the scrub, on
    /// the command, so it also proves explicit variables survive.
    const PROBE_MARKER_VAR: &str = "POHUNEK_TEST_ENV_PROBE_CHILD";

    /// Prefix of the lines the probe child prints.
    const PROBE_LINE: &str = "env-probe:";

    /// Session id the crafted parent environment carries.
    const HOST_SESSION_ID: &str = "s-host-session";

    /// Values of the crafted parent variables that must not reach a child.
    const SCRUBBED_HOST_VALUES: [&str; 25] = [
        HOST_SESSION_ID,
        "/run/host/pohunek.sock",
        "postgres://host/db",
        "/run/user/1000",
        "/host/share",
        "/home/host",
        "/host/tmp",
        "/host/repo",
        "/host/agent",
        "/host/bus",
        "/host/codex",
        SYNTHETIC_TOKEN,
        "/host/bash-env.sh",
        "/host/zdotdir",
        "/host/env.sh",
        "/host/python",
        "/host/startup.py",
        "/host/preload.so",
        "/host/registry-token",
        "/host/log-filter",
        "/host/target",
        "/host/cargo",
        "/host/rustup",
        "/host/lc-token",
        "/host/lc-api-key",
    ];

    /// Names of the crafted parent variables that must not reach a child.
    const SCRUBBED_NAMES: [&str; 20] = [
        "POHUNEK_SESSION_ID",
        "BASH_ENV",
        "ZDOTDIR",
        "ENV",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "GH_TOKEN",
        "LD_PRELOAD",
        "CARGO_REGISTRY_TOKEN",
        "RUST_LOG",
        "GIT_DIR",
        "SSH_AUTH_SOCK",
        "XDG_DATA_DIRS",
        "USER",
        "TERM",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "CARGO_TARGET_DIR",
        "LC_TOKEN",
        "LC_API_KEY",
    ];

    /// Synthetic credential the crafted parent carries; never a real secret.
    const SYNTHETIC_TOKEN: &str = "ghp_synthetic_token_value_0123456789";

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    fn crafted_parent() -> Vec<(OsString, OsString)> {
        vec![
            pair("POHUNEK_SESSION_ID", HOST_SESSION_ID),
            pair("POHUNEK_SOCKET_PATH", "/run/host/pohunek.sock"),
            pair("POHUNEK_RELAY_TEST_DATABASE_URL", "postgres://host/db"),
            pair("XDG_RUNTIME_DIR", "/run/user/1000"),
            pair("XDG_DATA_DIRS", "/host/share"),
            pair("HOME", "/home/host"),
            pair("TMPDIR", "/host/tmp"),
            pair("GIT_DIR", "/host/repo/.git"),
            pair("SSH_AUTH_SOCK", "/host/agent"),
            pair("DBUS_SESSION_BUS_ADDRESS", "unix:path=/host/bus"),
            pair("CODEX_HOME", "/host/codex"),
            pair("GH_TOKEN", SYNTHETIC_TOKEN),
            pair("BASH_ENV", "/host/bash-env.sh"),
            pair("ZDOTDIR", "/host/zdotdir"),
            pair("ENV", "/host/env.sh"),
            pair("PYTHONPATH", "/host/python"),
            pair("PYTHONSTARTUP", "/host/startup.py"),
            pair("LD_PRELOAD", "/host/preload.so"),
            pair("CARGO_REGISTRY_TOKEN", "/host/registry-token"),
            pair("RUST_LOG", "/host/log-filter"),
            pair("USER", "host-user"),
            pair("TERM", "xterm-host"),
            pair("PATH", "/host/bin:/usr/bin"),
            pair("CARGO_TARGET_DIR", "/host/target"),
            pair("CARGO_HOME", "/host/cargo"),
            pair("RUSTUP_HOME", "/host/rustup"),
            pair("LC_TOKEN", "/host/lc-token"),
            pair("LC_API_KEY", "/host/lc-api-key"),
            pair("RUST_BACKTRACE", "1"),
            pair("LANG", "C.UTF-8"),
            pair("LC_ALL", "C.UTF-8"),
        ]
    }

    #[test]
    fn root_is_removed_when_the_test_panics() {
        let mut root = None;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let env = TestEnv::new().expect("create env");
            root = Some(env.root().to_path_buf());
            std::fs::write(env.home().join("file"), b"x").expect("write file");
            panic!("test body failed");
        }));
        assert!(outcome.is_err());
        let root = root.expect("env was created before the panic");
        assert!(!root.exists(), "{} survived the panic", root.display());
    }

    #[test]
    fn scrub_passes_only_allowlisted_variables() {
        let env = scrubbed(crafted_parent(), []);
        let names: Vec<&str> = env.keys().filter_map(|name| name.to_str()).collect();
        assert_eq!(names, ["LANG", "LC_ALL", "PATH", "RUST_BACKTRACE"]);
        assert_eq!(env[OsStr::new("PATH")], "/host/bin:/usr/bin");
        for name in SCRUBBED_NAMES {
            assert!(
                !env.contains_key(OsStr::new(name)),
                "{name} reached a child"
            );
        }
    }

    #[test]
    fn scrub_keeps_values_that_are_not_utf8() {
        let name = OsString::from("LANG");
        let value = OsString::from_vec(vec![0xff, 0xfe]);
        let env = scrubbed([(name.clone(), value.clone())], []);
        assert_eq!(env[&name], value);
    }

    #[test]
    fn scrub_matches_names_exactly_and_prefixes_case_sensitively() {
        let env = scrubbed(
            [
                pair("lc_lower", "dropped"),
                pair("LCX", "dropped"),
                pair("LANGUAGES", "dropped"),
                pair("MYPATH", "dropped"),
                pair("PATHEXT", "dropped"),
                pair("CARGO_HOME_EXTRA", "dropped"),
                pair("LC_CTYPE", "kept"),
                pair("LC_", "dropped"),
                pair("LC_TOKEN", "dropped"),
                pair("LC_API_KEY", "dropped"),
            ],
            [],
        );
        let names: Vec<&str> = env.keys().filter_map(|name| name.to_str()).collect();
        assert_eq!(names, ["LC_CTYPE"]);
    }

    #[test]
    fn debug_output_names_variables_but_never_prints_values() {
        let env = TestEnv::with_parent_environment(crafted_parent()).expect("create env");
        let rendered = format!("{env:?}");
        assert!(rendered.contains("PATH"), "{rendered}");
        assert!(
            rendered.contains(&env.root().display().to_string()),
            "{rendered}"
        );
        for value in
            SCRUBBED_HOST_VALUES
                .iter()
                .chain(&["/host/bin", "/host/target", "/host/cargo"])
        {
            assert!(!rendered.contains(value), "{value} leaked into {rendered}");
        }
        assert!(!rendered.contains("GH_TOKEN"), "{rendered}");
    }

    #[test]
    fn env_pins_home_xdg_and_tmpdir_to_private_directories() {
        let env = TestEnv::with_parent_environment(crafted_parent()).expect("create env");
        let expected = [
            ("HOME", env.home()),
            ("XDG_CONFIG_HOME", env.config_home()),
            ("XDG_DATA_HOME", env.data_home()),
            ("XDG_STATE_HOME", env.state_home()),
            ("XDG_CACHE_HOME", env.cache_home()),
            ("XDG_RUNTIME_DIR", env.runtime_dir()),
            ("TMPDIR", env.tmp_dir()),
        ];
        for (name, dir) in expected {
            assert_eq!(
                env.environment()[OsStr::new(name)],
                dir.as_os_str(),
                "{name}"
            );
        }
        assert!(!env
            .environment()
            .contains_key(OsStr::new("POHUNEK_SESSION_ID")));
        assert!(!env.environment().contains_key(OsStr::new("XDG_DATA_DIRS")));
    }

    #[test]
    fn directories_are_private_and_inside_the_root() {
        let env = TestEnv::new().expect("create env");
        let dirs = [
            env.cwd(),
            env.home(),
            env.config_home(),
            env.data_home(),
            env.state_home(),
            env.cache_home(),
            env.runtime_dir(),
            env.tmp_dir(),
        ];
        for dir in dirs {
            assert!(
                dir.starts_with(env.root()) && dir != env.root(),
                "{}",
                dir.display()
            );
            let mode = std::fs::metadata(dir)
                .expect("stat dir")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, PRIVATE_DIR_MODE, "{}", dir.display());
        }
        let mode = std::fs::metadata(env.root())
            .expect("stat root")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_DIR_MODE);
    }

    #[test]
    fn long_base_is_rejected_without_creating_a_root() {
        let outer = crate::tempdir().expect("create outer");
        let base = outer
            .path()
            .join("b".repeat(STRICTEST_SOCKET_PATH_CAPACITY / 2));
        std::fs::create_dir(&base).expect("create long base");
        let error = TestEnv::with_base(&base, []).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("TMPDIR"), "{message}");
        assert!(message.contains(&base.display().to_string()), "{message}");
        assert!(
            message.contains(&STRICTEST_SOCKET_PATH_CAPACITY.to_string()),
            "{message}"
        );
        let created = std::fs::read_dir(&base).expect("read base").count();
        assert_eq!(created, 0, "a root was created below a rejected base");
    }

    #[test]
    fn symlinked_base_is_rejected() {
        let outer = crate::tempdir().expect("create outer");
        let real = outer.path().join("real");
        let link = outer.path().join("link");
        std::fs::create_dir(&real).expect("create real base");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        let error = TestEnv::with_base(&link, []).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("TMPDIR"), "{message}");
        assert!(message.contains(&link.display().to_string()), "{message}");
        assert!(message.contains("not canonical"), "{message}");
        // The target may still be too long for the base budget (a 12-byte
        // `temp_root` plus the fixture directory exceeds it), so only the
        // canonical check is asserted for it.
        if let Err(error) = TestEnv::with_base(&real, []) {
            assert!(!error.to_string().contains("not canonical"), "{error}");
        }
    }

    #[test]
    fn missing_base_is_rejected_with_the_base_named() {
        let outer = crate::tempdir().expect("create outer");
        let missing = outer.path().join("missing");
        let error = TestEnv::with_base(&missing, []).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn nested_worker_socket_fits_the_strictest_limit() {
        let env = TestEnv::new().expect("create env");
        let socket = env
            .socket_path(NESTED_SOCKET_SUFFIX.trim_start_matches('/'))
            .expect("socket path fits");
        assert!(socket.starts_with(env.runtime_dir().parent().expect("root")));
        let len = socket.as_os_str().len();
        assert!(len < STRICTEST_SOCKET_PATH_CAPACITY, "{len}");
    }

    #[test]
    fn socket_path_rejects_a_path_that_does_not_fit() {
        let env = TestEnv::new().expect("create env");
        let long = "a".repeat(STRICTEST_SOCKET_PATH_CAPACITY);
        let error = env.socket_path(&long).unwrap_err();
        let SocketPathError::TooLong { path, len } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(path, &env.root().join(&long));
        assert_eq!(*len, path.as_os_str().len());
        assert!(error.to_string().contains(&len.to_string()), "{error}");
    }

    #[test]
    fn socket_path_rejects_paths_that_leave_the_root() {
        let env = TestEnv::new().expect("create env");
        for escaping in ["/run/x.sock", "../x.sock", "run/../../x.sock"] {
            assert_eq!(
                env.socket_path(escaping),
                Err(SocketPathError::EscapesRoot {
                    path: PathBuf::from(escaping)
                }),
                "{escaping}"
            );
        }
    }

    #[test]
    fn socket_path_limit_counts_the_terminating_nul() {
        let fits = format!("/{}", "a".repeat(STRICTEST_SOCKET_PATH_CAPACITY - 2));
        assert_eq!(fits.len(), STRICTEST_SOCKET_PATH_CAPACITY - 1);
        check_socket_path(Path::new(&fits)).expect("103 bytes plus NUL fit");
        let overflows = format!("{fits}a");
        assert!(matches!(
            check_socket_path(Path::new(&overflows)),
            Err(SocketPathError::TooLong { len, .. }) if len == STRICTEST_SOCKET_PATH_CAPACITY
        ));
    }

    /// Child half of [`child_observes_the_scrubbed_environment`]: when started
    /// with the marker variable it prints its working directory and
    /// environment, otherwise it does nothing.
    #[test]
    fn probe_child_prints_its_environment() {
        if std::env::var_os(PROBE_MARKER_VAR).is_none() {
            return;
        }
        let cwd = std::env::current_dir().expect("child cwd");
        println!("{PROBE_LINE}cwd={}", cwd.display());
        for (name, value) in std::env::vars_os() {
            println!(
                "{PROBE_LINE}var={}={}",
                name.to_string_lossy(),
                value.to_string_lossy()
            );
        }
    }

    #[test]
    fn child_observes_the_scrubbed_environment() {
        let env = TestEnv::with_parent_environment(crafted_parent()).expect("create env");
        let exe = std::env::current_exe().expect("test executable");
        let output = env
            .command(exe)
            .args([
                "--exact",
                "env::tests::probe_child_prints_its_environment",
                "--nocapture",
            ])
            .env(PROBE_MARKER_VAR, "1")
            .output()
            .expect("run probe child");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let lines: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix(PROBE_LINE))
            .collect();
        let cwd = format!("cwd={}", env.cwd().display());
        assert!(lines.contains(&cwd.as_str()), "{lines:?}");
        let home = format!("var=HOME={}", env.home().display());
        assert!(lines.contains(&home.as_str()), "{lines:?}");
        for kept in [
            "var=LANG=C.UTF-8",
            "var=LC_ALL=C.UTF-8",
            "var=PATH=/host/bin:/usr/bin",
            "var=RUST_BACKTRACE=1",
        ] {
            assert!(lines.contains(&kept), "{kept} missing from {lines:?}");
        }
        for name in SCRUBBED_NAMES {
            let prefix = format!("var={name}=");
            assert!(
                !lines.iter().any(|line| line.starts_with(&prefix)),
                "{name} reached the child: {lines:?}"
            );
        }
        assert!(
            lines.contains(&"var=POHUNEK_TEST_ENV_PROBE_CHILD=1"),
            "{lines:?}"
        );
        let leaked: Vec<&&str> = lines
            .iter()
            .filter(|line| {
                SCRUBBED_HOST_VALUES
                    .iter()
                    .any(|value| line.contains(value))
            })
            .collect();
        assert!(leaked.is_empty(), "{leaked:?}");
    }

    #[tokio::test]
    async fn tokio_command_carries_the_scrubbed_environment() {
        let env = TestEnv::with_parent_environment(crafted_parent()).expect("create env");
        let exe = std::env::current_exe().expect("test executable");
        let output = env
            .tokio_command(exe)
            .args([
                "--exact",
                "env::tests::probe_child_prints_its_environment",
                "--nocapture",
            ])
            .env(PROBE_MARKER_VAR, "1")
            .output()
            .await
            .expect("run probe child");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert!(
            stdout.contains(&format!("{PROBE_LINE}cwd={}", env.cwd().display())),
            "{stdout}"
        );
        assert!(!stdout.contains(HOST_SESSION_ID), "{stdout}");
    }

    /// Prefix of the lines the umask probe child prints.
    const UMASK_PROBE_LINE: &str = "umask-probe:";

    /// Umask that removes every permission bit, the worst case for `mkdir`.
    const OWNER_MASKING_UMASK: &str = "777";

    /// Child half of [`directories_are_private_under_an_owner_masking_umask`].
    #[test]
    #[ignore = "child process of directories_are_private_under_an_owner_masking_umask"]
    fn umask_probe_test_env() {
        let env = TestEnv::new().expect("create env under the masking umask");
        std::fs::write(env.cwd().join("file"), b"x").expect("write into the cwd");
        for dir in [
            env.root(),
            env.cwd(),
            env.home(),
            env.config_home(),
            env.data_home(),
            env.state_home(),
            env.cache_home(),
            env.runtime_dir(),
            env.tmp_dir(),
        ] {
            let mode = std::fs::metadata(dir)
                .expect("stat dir")
                .permissions()
                .mode();
            println!("{UMASK_PROBE_LINE}{:o}", mode & 0o777);
        }
    }

    #[test]
    fn directories_are_private_under_an_owner_masking_umask() {
        let exe = std::env::current_exe().expect("test executable");
        // The umask is process-global, so it is set in a child; POSIX `sh` has the
        // `umask` builtin and exists at `/bin/sh` on Linux and macOS.
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("umask {OWNER_MASKING_UMASK}; exec \"$0\" \"$@\""),
            ])
            .arg(exe)
            .args([
                "--ignored",
                "--exact",
                "env::tests::umask_probe_test_env",
                "--nocapture",
            ])
            .output()
            .expect("run probe child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "probe failed: {stderr}");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let modes: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix(UMASK_PROBE_LINE))
            .collect();
        let expected = format!("{PRIVATE_DIR_MODE:o}");
        assert_eq!(modes, [expected.as_str(); 9], "{stdout}");
    }
}
