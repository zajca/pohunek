//! A hermetic per-test environment: private directories and a scrubbed
//! process environment.
//!
//! A test that spawns processes or builds trusted directories must not depend
//! on, or write into, state that belongs to the host or to a sibling test.
//! [`TestEnv`] owns, for one test:
//!
//! - a short private root under [`crate::temp_root`], removed when the
//!   [`TestEnv`] drops (also while a panic unwinds);
//! - a private working directory, `HOME`, the five XDG base directories and a
//!   `TMPDIR`, all inside that root and owner-only (`0700`);
//! - an environment for child processes that is the parent environment minus
//!   every variable that steers a process towards host state, with `HOME`,
//!   `XDG_*` and `TMPDIR` pointed at the private directories.
//!
//! The scrub applies to children only. Library code that reads the process
//! environment in-process, such as `std::env::var_os("POHUNEK_SESSION_ID")`,
//! still sees the test process's own variables; a `TestEnv` cannot change that.
//!
//! Children are started through [`TestEnv::command`] or
//! [`TestEnv::tokio_command`]. Both clear the inherited environment, so a
//! variable a child needs on purpose, such as `POHUNEK_WORKER_BIN`, is added
//! explicitly on the returned command and then survives the scrub.
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
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

/// Capacity of a Unix socket path on the strictest supported platform, in bytes.
///
/// Darwin's `sun_path` holds 104 bytes including the terminating NUL (Linux
/// holds 108). A path that fits this limit binds on every supported platform;
/// raising the value lets tests pass on Linux and fail on macOS.
pub const STRICTEST_SOCKET_PATH_CAPACITY: usize = 104;

/// Mode of every directory a [`TestEnv`] creates.
///
/// The trusted filesystem layer rejects group- or world-accessible private
/// roots, and `XDG_RUNTIME_DIR` must be owner-only by specification.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Name prefix of the [`TestEnv`] root.
///
/// Short because every byte of the root counts against
/// [`STRICTEST_SOCKET_PATH_CAPACITY`] for each socket bound beneath it.
const ROOT_PREFIX: &str = "ph-";

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

/// Variable-name prefixes removed from every child environment.
///
/// - `POHUNEK_`: session, daemon, socket and worker identity of an enclosing
///   pohunek session, and every test fixture override. A child that inherits a
///   session id without a daemon id fails origin validation.
/// - `XDG_`: base-directory and session variables of the host user. The five
///   base directories are re-pinned to private directories; the rest
///   (`XDG_DATA_DIRS`, `XDG_CONFIG_DIRS`, `XDG_SESSION_*`, ...) fall back to
///   their specification defaults.
/// - `GIT_`: `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and `GIT_CONFIG_*`
///   point git at the repository or configuration of the process that started
///   the test, for example when it runs under a git hook.
pub const SCRUBBED_PREFIXES: &[&str] = &["POHUNEK_", "XDG_", "GIT_"];

/// Exact variable names removed from every child environment.
///
/// Each one steers a child towards host state or a host service:
/// `HOME` and `TMPDIR` are replaced by private directories; `PWD` and `OLDPWD`
/// describe the directory the test was started in, not the child's working
/// directory; `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `GPG_AGENT_INFO` and
/// `GNUPGHOME` reach the host's credential agents; `DBUS_SESSION_BUS_ADDRESS`
/// and `DBUS_SYSTEM_BUS_ADDRESS` reach the host's secret service and system
/// services; `NOTIFY_SOCKET` makes a child report readiness to the host's
/// systemd; `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `HERMES_HOME` and
/// `UV_CACHE_DIR` redirect the agent integrations and the Python tool cache
/// that pohunek reads and writes to host directories.
///
/// Build and toolchain variables (`PATH`, `CARGO_*`, `RUSTUP_*`, `RUST*`,
/// `LD_*`, `DYLD_*`) and locale variables are kept: children such as `cargo`
/// need them, and none of them reaches pohunek's own state. Because `HOME` is
/// private, a test that spawns `cargo` or `rustup` must pass `CARGO_HOME` and
/// `RUSTUP_HOME` through its parent environment.
pub const SCRUBBED_VARS: &[&str] = &[
    HOME_VAR,
    TMPDIR_VAR,
    "PWD",
    "OLDPWD",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GPG_AGENT_INFO",
    "GNUPGHOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_SYSTEM_BUS_ADDRESS",
    "NOTIFY_SOCKET",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "HERMES_HOME",
    "UV_CACHE_DIR",
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
#[derive(Debug)]
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

impl TestEnv {
    /// Creates a private environment derived from the current process environment.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the root or one of its directories cannot be
    /// created.
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
    /// Returns the I/O error when the root or one of its directories cannot be
    /// created.
    pub fn with_parent_environment(
        parent: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> std::io::Result<Self> {
        let root = crate::tempdir_with_prefix(ROOT_PREFIX)?;
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

/// Returns whether `name` is removed from a child environment.
fn is_scrubbed(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    SCRUBBED_PREFIXES
        .iter()
        .any(|prefix| bytes.starts_with(prefix.as_bytes()))
        || SCRUBBED_VARS.iter().any(|var| bytes == var.as_bytes())
}

/// Builds a child environment: `parent` without the scrubbed variables, plus `pinned`.
fn scrubbed(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    pinned: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    parent
        .into_iter()
        .filter(|(name, _)| !is_scrubbed(name))
        .chain(pinned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::PermissionsExt as _;
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
    const SCRUBBED_HOST_VALUES: [&str; 11] = [
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
    ];

    /// Longest socket suffix the daemon tests nest below a root:
    /// `run/pohunek/workers/<36-character id>/control.sock`.
    const NESTED_SOCKET: &str =
        "run/pohunek/workers/0123456789abcdef0123456789abcdef0123/control.sock";

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
            pair("PATH", "/host/bin:/usr/bin"),
            pair("CARGO_TARGET_DIR", "/host/target"),
            pair("LANG", "C.UTF-8"),
        ]
    }

    #[test]
    fn root_is_removed_when_the_env_drops() {
        let env = TestEnv::new().expect("create env");
        let root = env.root().to_path_buf();
        std::fs::write(env.cwd().join("file"), b"x").expect("write file");
        assert!(root.is_dir());
        drop(env);
        assert!(!root.exists());
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
    fn scrub_removes_inherited_control_variables_and_keeps_build_variables() {
        let env = scrubbed(crafted_parent(), []);
        let names: Vec<&str> = env.keys().filter_map(|name| name.to_str()).collect();
        assert_eq!(names, ["CARGO_TARGET_DIR", "LANG", "PATH"]);
        assert_eq!(env[OsStr::new("PATH")], "/host/bin:/usr/bin");
    }

    #[test]
    fn scrub_pins_override_inherited_values() {
        let env = scrubbed(crafted_parent(), [pair("HOME", "/private/home")]);
        assert_eq!(env[OsStr::new("HOME")], "/private/home");
    }

    #[test]
    fn scrub_keeps_values_that_are_not_utf8() {
        let name = OsString::from("RAW_VAR");
        let value = OsString::from_vec(vec![0xff, 0xfe]);
        let env = scrubbed([(name.clone(), value.clone())], []);
        assert_eq!(env[&name], value);
    }

    #[test]
    fn scrub_matches_prefixes_case_sensitively_and_by_whole_name() {
        let env = scrubbed(
            [
                pair("pohunek_lower", "kept"),
                pair("POHUNEK", "kept"),
                pair("MY_HOME", "kept"),
                pair("HOMEPATH", "kept"),
            ],
            [],
        );
        assert_eq!(env.len(), 4);
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
    fn environments_do_not_share_directories() {
        let first = TestEnv::new().expect("create first");
        let second = TestEnv::new().expect("create second");
        assert_ne!(first.root(), second.root());
    }

    #[test]
    fn nested_worker_socket_fits_the_strictest_limit() {
        let env = TestEnv::new().expect("create env");
        let socket = env.socket_path(NESTED_SOCKET).expect("socket path fits");
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
        assert!(lines.contains(&"var=LANG=C.UTF-8"), "{lines:?}");
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
}
