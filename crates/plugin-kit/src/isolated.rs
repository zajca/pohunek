//! The isolated daemon the kit `prove` and `smoke` commands run on.
//!
//! One private root holds a home, XDG directories, a copy of the release
//! executables with the trust anchor and the index channel file beside the
//! daemon, and the daemon's control socket. Every child process runs with a
//! cleared environment, so nothing of the caller's `POHUNEK_*` variables,
//! sockets or home directory reaches it. The `xtask` canary capture uses
//! this module through the kit crate.

// Rust guideline compliant 2026-11-06

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::io::AsFd as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use package::{ANCHOR_FILE_NAME, INDEX_CHANNEL_FILE_NAME};
use tempfile::TempDir;

/// The release executables the host runs, by file name in `--release-dir`.
const BINARIES: [&str; 3] = ["pohunek", "pohunekd", "pohunek-sessiond"];

/// Mode of the prefix and of the installed executables: the service
/// installer's public mode. The daemon refuses an anchor whose directory chain
/// group or others can write.
const PUBLIC_MODE: u32 = 0o755;

/// Mode of the public documents beside the daemon.
const DOCUMENT_MODE: u32 = 0o644;

/// Mode of the runtime directory: the daemon requires an owner-only one.
const RUNTIME_DIRECTORY_MODE: u32 = 0o700;

/// Prefix of the isolation root's directory name.
///
/// Budget: the longest socket below the root is the staged worker path,
/// the root plus [`WORKER_SOCKET_TAIL_BYTES`] bytes. macOS allows 103
/// path bytes, so below the 12-byte `/private/tmp` parent the name holds
/// at most 14 bytes: `pk-` plus 6 random characters (9 bytes) leaves 5
/// bytes of margin. Linux allows 107, so the same name fits its
/// `/tmp` fallback with wider margin.
pub const ROOT_PREFIX: &str = "pk-";

/// Random characters `tempfile` appends to the root's name.
pub const ROOT_RANDOM_BYTES: usize = 6;

/// System-owned short parent tried when `$TMPDIR` is too long for a socket
/// path (macOS `$TMPDIR` is).
pub const SHORT_TEMP_PARENT: &str = "/tmp";

/// Short parent as macOS resolves it: `/tmp` is a symbolic link to
/// `/private/tmp`, so a fallback root lands below this canonical parent.
const MACOS_SHORT_TEMP_PARENT: &str = "/private/tmp";

/// Bytes the longest socket below a root adds to the root's path: the
/// staged worker bind (`xdg-runtime/<app>/workers/<session id>/.s<hex>`),
/// the longest of the daemon and worker paths the daemon validates.
const WORKER_SOCKET_TAIL_BYTES: usize = 1
    + RUNTIME_DIRECTORY.len()
    + 1
    + pohunek_paths::APP_DIR.len()
    + 1
    + pohunek_paths::WORKERS_SUBDIR.len()
    + 1
    + pohunek_paths::MAX_WORKER_SESSION_ID_BYTES
    + 1
    + pohunek_paths::STAGED_SOCKET_PREFIX.len()
    + pohunek_paths::STAGED_SOCKET_SUFFIX_CHARS;

/// Every fallback root must leave room for the longest socket on macOS:
/// the check runs on every platform at compile time, so a longer prefix
/// fails the build instead of macOS CI.
const _: () = assert!(
    MACOS_SHORT_TEMP_PARENT.len()
        + 1
        + ROOT_PREFIX.len()
        + ROOT_RANDOM_BYTES
        + WORKER_SOCKET_TAIL_BYTES
        <= pohunek_paths::DARWIN_SOCKET_PATH_MAX_BYTES
);

/// Socket path below the root: `XDG_RUNTIME_DIR` plus the application
/// directory and socket file the daemon appends to it. Bare `pohunek`
/// commands reach the isolated daemon through this directory: the CLI
/// resolves the socket from the runtime directory, never from the socket
/// variable below. The variable stays exported only as information for
/// hook-style clients that read it from a session payload.
const SOCKET_RELATIVE_PATH: &str = "xdg-runtime/pohunek/daemon.sock";

/// Environment variable naming the isolated daemon's control socket for a
/// smoke script: informational only, for hook-style clients that read the
/// socket path from the environment. A bare `pohunek` in the script
/// reaches the isolated daemon through the isolated `XDG_RUNTIME_DIR`,
/// not through this variable (the CLI resolves the socket from the
/// runtime directory).
const SOCKET_ENV_VAR: &str = protocol::ENV_SOCKET_PATH;

/// Environment variable handing the smoke script the live session id the
/// generic smoke passed on: a kit-owned name, not the handshake variable.
/// The origin pair (`POHUNEK_SESSION_ID` / `POHUNEK_DAEMON_ID`) stays
/// absent, so the script's `pohunek` commands connect as an owner with an
/// explicit session id, never as the session.
const SESSION_ENV_VAR: &str = "POHUNEK_SMOKE_SESSION";

/// Prefix of a session root's directory name: a root that runs sessions
/// nests worker sockets deeper than the daemon socket.
const SESSION_ROOT_PREFIX: &str = "pc-";

/// Random characters `tempfile` appends to a session root's name.
const SESSION_ROOT_RANDOM_BYTES: usize = 4;

/// Socket paths of this length or longer are refused: the platform's `sun_path`
/// holds the path and its terminator.
const SOCKET_PATH_MAX_BYTES: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

/// Longest a daemon may take to accept connections. It opens an empty store
/// and binds its socket within seconds; the bound ends a hung start on a
/// loaded runner.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest the daemon may take to exit after SIGTERM: it stops its workers
/// and flushes its event log.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Longest one CLI command may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Pause between two checks of a child process.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Largest output kept of one command; a JSON verdict is far smaller.
const OUTPUT_LIMIT_BYTES: u64 = 1024 * 1024;

/// Trailing daemon-log bytes a startup failure reports.
const LOG_TAIL_BYTES: u64 = 4 * 1024;

/// Placeholder that replaces the root path in reported text.
const ROOT_PLACEHOLDER: &str = "<ISOLATED_ROOT>";

/// Placeholder that replaces the caller's home directory in reported text.
const HOME_PLACEHOLDER: &str = "<USER_HOME>";

/// Command search path of every child: the system directories only.
const CHILD_PATH: &str = "/usr/bin:/bin";

/// The daemon flag that keeps the remote control listener off.
pub const LOCAL_ONLY_FLAG: &str = "--local-only";

/// What the daemon logs, before it accepts connections, when
/// [`LOCAL_ONLY_FLAG`] is in force.
pub const LOCAL_ONLY_LOG_LINE: &str = "remote control listener disabled by --local-only";

/// UTF-8 locale of every child; macOS ships no `C.UTF-8`.
const CHILD_LOCALE: &str = if cfg!(target_os = "macos") {
    "en_US.UTF-8"
} else {
    "C.UTF-8"
};

/// Shell the daemon hands to terminal sessions; the proof starts none.
const CHILD_SHELL: &str = "/bin/sh";

/// Variable selecting direct-child worker supervision, the documented
/// dev/test contract for a daemon without a service manager.
const LAUNCHER_VARIABLE: &str = "POHUNEK_WORKER_LAUNCHER";

/// Value of [`LAUNCHER_VARIABLE`] for supervision without a service manager.
const SUBPROCESS_LAUNCHER: &str = "subprocess";

/// File the daemon's combined output is written to.
const DAEMON_LOG_FILE: &str = "pohunekd.log";

/// Sub-directories of the root.
const PREFIX_DIRECTORY: &str = "prefix";
const WORK_DIRECTORY: &str = "work";
const RUNTIME_DIRECTORY: &str = "xdg-runtime";
const HOME_DIRECTORIES: [&str; 5] = ["home", "xdg-config", "xdg-cache", "xdg-data", "xdg-state"];

/// The isolation root, removed with its contents when [`Root::remove`] runs or
/// the value drops.
#[derive(Debug)]
pub struct Root {
    temp: TempDir,
}

impl Root {
    /// Creates the root with its private directories below a canonical parent
    /// whose daemon and worker socket paths fit the platform limit. A deep
    /// `TMPDIR` falls back to the short system parent, so an isolated host
    /// always runs where its session workers can bind.
    pub fn create() -> Result<Self, String> {
        let candidates = [std::env::temp_dir(), PathBuf::from(SHORT_TEMP_PARENT)];
        let mut last = String::from("no temporary directory is available");
        for parent in candidates {
            let parent = match Self::check_session_parent(&parent) {
                Ok(parent) => parent,
                Err(error) => {
                    last = error;
                    continue;
                }
            };
            let temp = match tempfile::Builder::new()
                .prefix(ROOT_PREFIX)
                .rand_bytes(ROOT_RANDOM_BYTES)
                .tempdir_in(&parent)
            {
                Ok(temp) => temp,
                Err(error) => {
                    last = format!("cannot create the isolation root: {error}");
                    continue;
                }
            };
            // The parent check used the shorter session placeholder; the
            // proof prefix is longer, so validate the real paths too and
            // fall back to the short parent when they do not fit.
            if let Err(error) = Self::check_root_sockets(temp.path()) {
                last = error;
                continue;
            }
            if temp.path().join(SOCKET_RELATIVE_PATH).as_os_str().len() >= SOCKET_PATH_MAX_BYTES {
                "the daemon socket path would exceed the platform limit; set TMPDIR to a shorter directory"
                    .clone_into(&mut last);
                continue;
            }
            let root = Self { temp };
            root.create_directories()?;
            return Ok(root);
        }
        Err(last)
    }

    /// Creates a root below `parent`, after [`Root::check_session_parent`]
    /// accepted it. There is no fallback parent: the caller names the
    /// directory, so a scenario can keep the root inside its own temporary
    /// directory and assert the cleanup from the outside. The parent check
    /// used the shorter session placeholder, so the real root is rechecked
    /// too; its name differs from the placeholder.
    pub fn create_in(parent: &Path) -> Result<Self, String> {
        let parent = Self::check_session_parent(parent)?;
        let temp = tempfile::Builder::new()
            .prefix(ROOT_PREFIX)
            .rand_bytes(ROOT_RANDOM_BYTES)
            .tempdir_in(&parent)
            .map_err(|error| format!("cannot create the isolation root: {error}"))?;
        Self::check_root_sockets(temp.path())?;
        let root = Self { temp };
        root.create_directories()?;
        Ok(root)
    }

    /// Creates a root that runs agent sessions directly below `parent`, after
    /// [`Root::check_session_parent`] accepted it. There is no fallback
    /// parent: the caller names the directory.
    pub fn create_for_sessions(parent: &Path) -> Result<Self, String> {
        let parent = Self::check_session_parent(parent)?;
        let temp = tempfile::Builder::new()
            .prefix(SESSION_ROOT_PREFIX)
            .rand_bytes(SESSION_ROOT_RANDOM_BYTES)
            .tempdir_in(&parent)
            .map_err(|error| format!("cannot create the isolation root: {error}"))?;
        let root = Self { temp };
        root.create_directories()?;
        Ok(root)
    }

    /// Resolves `parent` and checks that a session root below it leaves room
    /// for every socket the daemon and its session workers bind. The check
    /// is the daemon's own: its paths resolved from the root's environment,
    /// and the longest worker socket of the longest session ID validated with
    /// its staged bind name, as the daemon does before it spawns a worker.
    /// Returns the canonical parent; nothing is created.
    pub fn check_session_parent(parent: &Path) -> Result<PathBuf, String> {
        let parent = fs::canonicalize(parent)
            .map_err(|error| format!("cannot resolve `{}`: {error}", parent.display()))?;
        // A placeholder of the root's name length stands for the random name.
        let name = format!(
            "{SESSION_ROOT_PREFIX}{}",
            "0".repeat(SESSION_ROOT_RANDOM_BYTES)
        );
        Self::check_root_sockets(&parent.join(name))?;
        Ok(parent)
    }

    /// Checks that the daemon and worker socket paths below `root` fit the
    /// platform limit, with the daemon's own path resolution and the longest
    /// worker session ID.
    fn check_root_sockets(root: &Path) -> Result<(), String> {
        let platform = pohunek_paths::Platform::current().map_err(|error| error.to_string())?;
        let display = root.display().to_string();
        let environment = root_environment(root);
        let variable = |name: &str| {
            environment
                .iter()
                .find(|(key, _value)| key == name)
                .map(|(_key, value)| value.clone())
        };
        let path_env = pohunek_paths::PathEnv {
            xdg_runtime_dir: variable("XDG_RUNTIME_DIR"),
            xdg_data_home: variable("XDG_DATA_HOME"),
            xdg_state_home: variable("XDG_STATE_HOME"),
            xdg_cache_home: variable("XDG_CACHE_HOME"),
            xdg_config_home: variable("XDG_CONFIG_HOME"),
            home: variable("HOME"),
        };
        let too_long = |error: &dyn std::fmt::Display| {
            format!(
                "`{display}` is too long for the session sockets below it ({error}); choose a shorter directory"
            )
        };
        let paths = pohunek_paths::BasePaths::resolve_for(
            platform,
            rustix::process::geteuid().as_raw(),
            &path_env,
        )
        .map_err(|error| too_long(&error))?;
        let session_id = format!(
            "{}{}",
            pohunek_paths::WORKER_SESSION_ID_PREFIX,
            "0".repeat(
                pohunek_paths::MAX_WORKER_SESSION_ID_BYTES
                    - pohunek_paths::WORKER_SESSION_ID_PREFIX.len()
            )
        );
        match pohunek_paths::worker_socket_path(&paths.worker_runtime_root(), &session_id, platform)
        {
            Ok(Some(_socket)) => Ok(()),
            Ok(None) => Err(format!(
                "`{session_id}` is not a worker session ID; the socket check cannot run"
            )),
            Err(error) => Err(too_long(&error)),
        }
    }

    fn create_directories(&self) -> Result<(), String> {
        let make = |name: &str, mode: u32| {
            fs::DirBuilder::new()
                .mode(mode)
                .create(self.path().join(name))
                .map_err(|error| format!("cannot create the isolated `{name}` directory: {error}"))
        };
        for name in HOME_DIRECTORIES {
            make(name, RUNTIME_DIRECTORY_MODE)?;
        }
        make(RUNTIME_DIRECTORY, RUNTIME_DIRECTORY_MODE)?;
        make(WORK_DIRECTORY, RUNTIME_DIRECTORY_MODE)
    }

    /// The root directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.temp.path()
    }

    /// The isolated `XDG_CONFIG_HOME`.
    #[must_use]
    pub fn config_home(&self) -> PathBuf {
        self.path().join("xdg-config")
    }

    /// The file the daemon's combined output is written to.
    #[must_use]
    pub fn daemon_log(&self) -> PathBuf {
        self.path().join(DAEMON_LOG_FILE)
    }

    /// Removes the root and its contents.
    pub fn remove(self) -> Result<(), String> {
        self.temp
            .close()
            .map_err(|error| format!("cannot remove the isolation root: {error}"))
    }

    /// The environment of every child process.
    fn environment(&self) -> Vec<(OsString, OsString)> {
        root_environment(self.path())
    }

    /// Replaces the root path in `text`.
    fn sanitize(&self, text: &str) -> String {
        let text = text.replace(&*self.path().to_string_lossy(), ROOT_PLACEHOLDER);
        match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => {
                text.replace(&*home.to_string_lossy(), HOME_PLACEHOLDER)
            }
            _ => text,
        }
    }
}

/// The environment of every child process of a root at `root`.
fn root_environment(root: &Path) -> Vec<(OsString, OsString)> {
    let path = |name: &str| root.join(name).into_os_string();
    vec![
        ("HOME".into(), path("home")),
        ("XDG_CONFIG_HOME".into(), path("xdg-config")),
        ("XDG_CACHE_HOME".into(), path("xdg-cache")),
        ("XDG_DATA_HOME".into(), path("xdg-data")),
        ("XDG_STATE_HOME".into(), path("xdg-state")),
        ("XDG_RUNTIME_DIR".into(), path(RUNTIME_DIRECTORY)),
        ("PATH".into(), CHILD_PATH.into()),
        ("LANG".into(), CHILD_LOCALE.into()),
        ("LC_ALL".into(), CHILD_LOCALE.into()),
        ("NO_COLOR".into(), "1".into()),
        ("TERM".into(), "dumb".into()),
    ]
}

/// What a finished CLI command left.
#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
}

/// A running copy of the release daemon and the CLI copy that talks to it.
/// Dropping it ends the daemon's whole process group.
#[derive(Debug)]
pub struct Host {
    root: PathBuf,
    prefix: PathBuf,
    environment: Vec<(OsString, OsString)>,
    daemon: Option<Child>,
    commands: usize,
}

impl Host {
    /// Copies the release executables from `release_dir` into the prefix
    /// below `root` and puts `anchor` and `channel` beside the daemon. No
    /// process runs yet.
    pub fn prepare(
        root: &Root,
        release_dir: &Path,
        anchor: &Path,
        channel: &Path,
    ) -> Result<Self, String> {
        let release_dir = fs::canonicalize(release_dir)
            .map_err(|error| format!("cannot resolve --release-dir: {error}"))?;
        let prefix = root.path().join(PREFIX_DIRECTORY);
        fs::DirBuilder::new()
            .mode(PUBLIC_MODE)
            .create(&prefix)
            .map_err(|error| format!("cannot create the prefix: {error}"))?;
        for name in BINARIES {
            let source = release_dir.join(name);
            require_safe_executable(&source, name)?;
            let installed = prefix.join(name);
            fs::copy(&source, &installed)
                .and_then(|_bytes| {
                    fs::set_permissions(&installed, fs::Permissions::from_mode(PUBLIC_MODE))
                })
                .map_err(|error| format!("cannot install `{name}` into the prefix: {error}"))?;
        }
        for (source, name) in [
            (anchor, ANCHOR_FILE_NAME),
            (channel, INDEX_CHANNEL_FILE_NAME),
        ] {
            let installed = prefix.join(name);
            fs::copy(source, &installed)
                .and_then(|_bytes| {
                    fs::set_permissions(&installed, fs::Permissions::from_mode(DOCUMENT_MODE))
                })
                .map_err(|error| format!("cannot install `{name}` into the prefix: {error}"))?;
        }

        Ok(Self {
            root: root.path().to_path_buf(),
            prefix,
            environment: root.environment(),
            daemon: None,
            commands: 0,
        })
    }

    /// Puts `upstream` in front of the `PATH` every child of this host sees,
    /// the way an installed upstream's directory stands in front of the
    /// daemon's own `PATH`: the daemon resolves the runtime program through
    /// `executable_search_path`, so it finds an upstream program there
    /// first. Accepts a directory or an executable inside one (its parent
    /// directory is used); refuses anything else.
    pub fn prepend_path(&mut self, upstream: &Path) -> Result<(), String> {
        let resolved = fs::canonicalize(upstream)
            .map_err(|error| format!("cannot resolve `--upstream`: {error}"))?;
        let dir = if resolved.is_dir() {
            resolved
        } else if resolved.is_file() {
            resolved
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| format!("`--upstream` `{}` has no parent", resolved.display()))?
        } else {
            return Err(format!(
                "`--upstream` `{}` is neither a directory nor a file",
                resolved.display()
            ));
        };
        let path = self
            .environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let mut dirs = vec![dir.into_os_string()];
        dirs.extend(std::env::split_paths(&path).map(PathBuf::into_os_string));
        let joined: OsString = std::env::join_paths(dirs)
            .map_err(|error| format!("cannot join the upstream `PATH`: {error}"))?;
        for (key, value) in &mut self.environment {
            if key == "PATH" {
                value.clone_from(&joined);
            }
        }
        Ok(())
    }

    /// Starts the daemon of the prefix with `--local-only`, waits until it
    /// accepts connections and requires its log to confirm that the remote
    /// control listener is off: on a host with an overlay, the daemon would
    /// otherwise try to serve the control protocol on the host's overlay
    /// address.
    pub fn launch_local_only(&mut self, root: &Root) -> Result<(), String> {
        self.launch_with(root, &[LOCAL_ONLY_FLAG])?;
        let log = fs::read_to_string(root.path().join(DAEMON_LOG_FILE))
            .map_err(|error| format!("cannot read the daemon log: {error}"))?;
        if log.contains(LOCAL_ONLY_LOG_LINE) {
            Ok(())
        } else {
            Err(format!(
                "the daemon log does not confirm `{LOCAL_ONLY_FLAG}` (`{LOCAL_ONLY_LOG_LINE}`)"
            ))
        }
    }

    /// Starts the daemon of the prefix with `arguments` and waits until it
    /// accepts connections.
    fn launch_with(&mut self, root: &Root, arguments: &[&str]) -> Result<(), String> {
        let log = File::create(root.path().join(DAEMON_LOG_FILE))
            .map_err(|error| format!("cannot create the daemon log: {error}"))?;
        let log_err = log
            .try_clone()
            .map_err(|error| format!("cannot duplicate the daemon log: {error}"))?;
        let mut command = Command::new(self.prefix.join("pohunekd"));
        command
            .args(arguments)
            .current_dir(root.path().join(WORK_DIRECTORY))
            .env_clear()
            .envs(self.environment.iter().cloned())
            .env("SHELL", CHILD_SHELL)
            .env(LAUNCHER_VARIABLE, SUBPROCESS_LAUNCHER)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .process_group(0);
        self.daemon = Some(
            command
                .spawn()
                .map_err(|error| format!("cannot start the daemon: {error}"))?,
        );
        self.wait_ready(root)
    }

    /// Runs `pohunekd --version` and returns its first output line.
    pub fn core_version(&mut self) -> Result<String, String> {
        let output = self.run_program("pohunekd", &["--version"])?;
        let text = String::from_utf8_lossy(&output.stdout);
        match (output.code, text.lines().next()) {
            (Some(0), Some(line)) if !line.trim().is_empty() => Ok(line.trim().to_owned()),
            _ => Err("`pohunekd --version` printed no version".to_owned()),
        }
    }

    /// The isolation root this host runs in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Runs the installed `pohunek` with `arguments`.
    pub fn cli(&mut self, arguments: &[&str]) -> Result<Output, String> {
        self.run_program("pohunek", arguments)
    }

    /// Runs `script`, a plugin-owned executable from the plugin repository,
    /// with the live session id exported under `POHUNEK_SMOKE_SESSION`. The
    /// script reaches the isolated daemon through the isolated
    /// `XDG_RUNTIME_DIR`: a bare `pohunek` on the script's `PATH` (the
    /// isolated prefix, holding the release executables, comes first)
    /// resolves its socket from that directory, so only the isolated
    /// daemon's sessions answer. `POHUNEK_SOCKET_PATH` is exported only
    /// as information for hook-style clients, not as routing. The origin
    /// pair the CLI would otherwise inherit
    /// (`POHUNEK_SESSION_ID`/`POHUNEK_DAEMON_ID`) is deliberately absent,
    /// so the script connects as an owner with an explicit id, not as
    /// the live session. The script's stdout and stderr both go to the
    /// kit's stderr unchanged, so `--json` stdout stays one JSON document.
    /// The script runs in the smoke's own cwd: the caller starts the kit
    /// in the plugin repository, and the kit never changes its cwd. After
    /// the script exits, its process group is killed on every path, so no
    /// backgrounded child survives the run.
    ///
    /// # Errors
    ///
    /// Returns an error when the script is missing, a symbolic link, not
    /// a regular file or not executable (the same bar as a release
    /// executable), or when it does not finish within [`COMMAND_TIMEOUT`].
    pub fn run_script(&mut self, script: &Path, session: &str) -> Result<ExitStatus, String> {
        require_safe_executable(script, &script.display().to_string())?;
        let socket = self.root.join(SOCKET_RELATIVE_PATH);
        let mut script_path = vec![self.prefix.clone().into_os_string()];
        script_path.extend(std::env::split_paths(&self.script_path()).map(PathBuf::into_os_string));
        let script_path = std::env::join_paths(script_path)
            .map_err(|error| format!("cannot join the smoke script `PATH`: {error}"))?;
        let extra = [
            (OsString::from(SOCKET_ENV_VAR), socket.into_os_string()),
            (OsString::from(SESSION_ENV_VAR), OsString::from(session)),
            (OsString::from("PATH"), script_path),
        ];
        let stderr = std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(Stdio::from)
            .map_err(|error| format!("cannot duplicate stderr for the smoke script: {error}"))?;
        let mut child = Command::new(script)
            .env_clear()
            .envs(self.environment.iter().cloned())
            .envs(extra.iter().cloned())
            .stdin(Stdio::null())
            .stdout(stderr)
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
            .map_err(|error| format!("cannot start the smoke script: {error}"))?;
        let status = wait_for_exit(&mut child, "the smoke script");
        // No backgrounded child outlives the run: `ESRCH` (nothing left)
        // is the normal path, anything else is ignored too because the
        // script already exited and there is nothing left to report.
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        status
    }

    /// The `PATH` every child except a smoke script runs with.
    fn script_path(&self) -> OsString {
        self.environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    /// Runs `program`, an executable outside the prefix, with `arguments` in
    /// the isolated environment plus `extra_environment`.
    pub fn run_external(
        &mut self,
        program: &Path,
        arguments: &[&str],
        extra_environment: &[(OsString, OsString)],
    ) -> Result<Output, String> {
        let label = program.file_name().map_or_else(
            || program.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.run_executable(program, &label, arguments, extra_environment)
    }

    fn run_program(&mut self, program: &str, arguments: &[&str]) -> Result<Output, String> {
        let executable = self.prefix.join(program);
        self.run_executable(&executable, program, arguments, &[])
    }

    fn run_executable(
        &mut self,
        executable: &Path,
        program: &str,
        arguments: &[&str],
        extra_environment: &[(OsString, OsString)],
    ) -> Result<Output, String> {
        let output_path = self.root.join(format!("command-{}.out", self.commands));
        self.commands += 1;
        let output = File::create(&output_path)
            .map_err(|error| format!("cannot create the command output file: {error}"))?;
        let mut child = Command::new(executable)
            .args(arguments)
            .current_dir(self.root.join(WORK_DIRECTORY))
            .env_clear()
            .envs(self.environment.iter().cloned())
            .envs(extra_environment.iter().cloned())
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| format!("cannot start `{program}`: {error}"))?;
        let status = wait_for_exit(&mut child, &format!("`{program}`"))?;
        let mut stdout = Vec::new();
        File::open(&output_path)
            .and_then(|file| file.take(OUTPUT_LIMIT_BYTES + 1).read_to_end(&mut stdout))
            .map_err(|error| format!("cannot read the output of `{program}`: {error}"))?;
        if u64::try_from(stdout.len()).map_or(true, |length| length > OUTPUT_LIMIT_BYTES) {
            return Err(format!(
                "`{program}` printed more than {OUTPUT_LIMIT_BYTES} bytes"
            ));
        }
        Ok(Output {
            code: status.code(),
            stdout,
        })
    }

    /// Polls the control socket until the daemon accepts a connection.
    fn wait_ready(&mut self, root: &Root) -> Result<(), String> {
        let socket = self.root.join(SOCKET_RELATIVE_PATH);
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self
                .daemon
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
            {
                // Reaped: its pid may be reused, so Drop must not signal it.
                self.daemon = None;
                return Err(format!(
                    "the daemon exited before it accepted connections ({status}):\n{}",
                    self.log_tail(root)
                ));
            }
            if UnixStream::connect(&socket).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the daemon did not accept connections within {READY_TIMEOUT:?}:\n{}",
                    self.log_tail(root)
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Signals the daemon with SIGTERM and requires a successful exit within
    /// [`STOP_TIMEOUT`].
    pub fn stop(mut self) -> Result<(), String> {
        let Some(mut daemon) = self.daemon.take() else {
            return Ok(());
        };
        let pid = i32::try_from(daemon.id())
            .map(Pid::from_raw)
            .map_err(|_error| "the daemon pid is out of range".to_owned())?;
        if let Err(error) = kill(pid, Signal::SIGTERM) {
            self.daemon = Some(daemon);
            return Err(format!("cannot signal the daemon: {error}"));
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match daemon.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(format!("the daemon did not exit cleanly ({status})"));
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    self.daemon = Some(daemon);
                    return Err(format!(
                        "the daemon did not exit within {STOP_TIMEOUT:?} of SIGTERM"
                    ));
                }
                Err(error) => {
                    self.daemon = Some(daemon);
                    return Err(format!("cannot wait for the daemon: {error}"));
                }
            }
        }
    }

    /// The sanitized tail of the daemon's combined output.
    fn log_tail(&self, root: &Root) -> String {
        let mut bytes = Vec::new();
        let read = File::open(self.root.join(DAEMON_LOG_FILE)).and_then(|file| {
            let length = file.metadata()?.len();
            let mut file = file;
            std::io::Seek::seek(
                &mut file,
                std::io::SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)),
            )?;
            file.read_to_end(&mut bytes)
        });
        match read {
            Ok(_length) => root.sanitize(&String::from_utf8_lossy(&bytes)),
            Err(_error) => "daemon log unavailable".to_owned(),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            // The daemon leads its own process group, so this also ends any
            // worker it started. It may have exited already.
            end_group(&mut daemon);
        }
    }
}

/// Waits for `child` until it exits or [`COMMAND_TIMEOUT`] passes, then
/// reaps its process group on a timeout or a wait error. Returns the exit
/// status; `what` names the child in the timeout and wait errors.
fn wait_for_exit(child: &mut Child, what: &str) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                end_group(child);
                return Err(format!("{what} did not finish within {COMMAND_TIMEOUT:?}"));
            }
            Err(error) => {
                end_group(child);
                return Err(format!("cannot wait for {what}: {error}"));
            }
        }
    }
}

/// Kills `child`'s process group and reaps `child`.
fn end_group(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Refuses an executable that is missing, a symbolic link, not a regular
/// file, not executable or writable by group or others.
fn require_safe_executable(path: &Path, name: &str) -> Result<(), String> {
    let refuse = |reason: &str| Err(format!("release executable `{name}` {reason}"));
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return refuse("does not exist");
    };
    if metadata.file_type().is_symlink() {
        return refuse("is a symbolic link");
    }
    if !metadata.is_file() {
        return refuse("is not a regular file");
    }
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 || mode & 0o022 != 0 {
        return refuse("has unsafe permissions");
    }
    Ok(())
}
