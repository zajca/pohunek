//! The isolated daemon `plugin prove` installs through.
//!
//! The root directory holds a private home, XDG directories, a copy of the
//! release executables with the trust anchor and the index channel file beside
//! the daemon, and the daemon's control socket. Every child process runs with
//! a cleared environment, so nothing of the caller's `POHUNEK_*` variables,
//! sockets or home directory reaches it.

// Rust guideline compliant 2026-11-06

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// Prefix of the isolation root's directory name; short because the control
/// socket path below the root must fit the platform limit.
const ROOT_PREFIX: &str = "pohunek-prove-";

/// Random characters `tempfile` appends to the root's name.
const ROOT_RANDOM_BYTES: usize = 8;

/// System-owned short parent tried when `$TMPDIR` is too long for a socket
/// path (macOS `$TMPDIR` is).
const SHORT_TEMP_PARENT: &str = "/tmp";

/// Socket path below the root: `XDG_RUNTIME_DIR` plus the application
/// directory and socket file the daemon appends to it.
const SOCKET_RELATIVE_PATH: &str = "xdg-runtime/pohunek/daemon.sock";

/// Longest Unix socket path the platform accepts, terminator included.
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

/// Command search path of every child: the system directories only.
const CHILD_PATH: &str = "/usr/bin:/bin";

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
pub(super) struct Root {
    temp: TempDir,
}

impl Root {
    /// Creates the root with its private directories below a canonical parent
    /// whose control socket path fits the platform limit.
    pub(super) fn create() -> Result<Self, String> {
        let candidates = [std::env::temp_dir(), PathBuf::from(SHORT_TEMP_PARENT)];
        let mut last = String::from("no temporary directory is available");
        for parent in candidates {
            let parent = match fs::canonicalize(&parent) {
                Ok(parent) => parent,
                Err(error) => {
                    last = format!("cannot resolve `{}`: {error}", parent.display());
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
    pub(super) fn path(&self) -> &Path {
        self.temp.path()
    }

    /// Removes the root and its contents.
    pub(super) fn remove(self) -> Result<(), String> {
        self.temp
            .close()
            .map_err(|error| format!("cannot remove the isolation root: {error}"))
    }

    /// The environment of every child process.
    fn environment(&self) -> Vec<(OsString, OsString)> {
        let path = |name: &str| self.path().join(name).into_os_string();
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

    /// Replaces the root path in `text`.
    fn sanitize(&self, text: &str) -> String {
        text.replace(&*self.path().to_string_lossy(), ROOT_PLACEHOLDER)
    }
}

/// What a finished CLI command left.
pub(super) struct Output {
    pub(super) code: Option<i32>,
    pub(super) stdout: Vec<u8>,
}

/// A running copy of the release daemon and the CLI copy that talks to it.
/// Dropping it ends the daemon's whole process group.
#[derive(Debug)]
pub(super) struct Host {
    root: PathBuf,
    prefix: PathBuf,
    environment: Vec<(OsString, OsString)>,
    daemon: Option<Child>,
    commands: usize,
}

impl Host {
    /// Copies the release executables from `release_dir` into the prefix
    /// below `root`, puts `anchor` and `channel` beside the daemon, and starts
    /// the daemon.
    pub(super) fn start(
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

        let log = File::create(root.path().join(DAEMON_LOG_FILE))
            .map_err(|error| format!("cannot create the daemon log: {error}"))?;
        let log_err = log
            .try_clone()
            .map_err(|error| format!("cannot duplicate the daemon log: {error}"))?;
        let environment = root.environment();
        let mut command = Command::new(prefix.join("pohunekd"));
        command
            .current_dir(root.path().join(WORK_DIRECTORY))
            .env_clear()
            .envs(environment.iter().cloned())
            .env("SHELL", CHILD_SHELL)
            .env(LAUNCHER_VARIABLE, SUBPROCESS_LAUNCHER)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .process_group(0);
        let daemon = command
            .spawn()
            .map_err(|error| format!("cannot start the daemon: {error}"))?;
        let mut host = Self {
            root: root.path().to_path_buf(),
            prefix,
            environment,
            daemon: Some(daemon),
            commands: 0,
        };
        host.wait_ready(root)?;
        Ok(host)
    }

    /// Runs `pohunekd --version` and returns its first output line.
    pub(super) fn core_version(&mut self) -> Result<String, String> {
        let output = self.run_program("pohunekd", &["--version"])?;
        let text = String::from_utf8_lossy(&output.stdout);
        match (output.code, text.lines().next()) {
            (Some(0), Some(line)) if !line.trim().is_empty() => Ok(line.trim().to_owned()),
            _ => Err("`pohunekd --version` printed no version".to_owned()),
        }
    }

    /// Runs the installed `pohunek` with `arguments`.
    pub(super) fn cli(&mut self, arguments: &[&str]) -> Result<Output, String> {
        self.run_program("pohunek", arguments)
    }

    fn run_program(&mut self, program: &str, arguments: &[&str]) -> Result<Output, String> {
        let output_path = self.root.join(format!("command-{}.out", self.commands));
        self.commands += 1;
        let output = File::create(&output_path)
            .map_err(|error| format!("cannot create the command output file: {error}"))?;
        let mut child = Command::new(self.prefix.join(program))
            .args(arguments)
            .current_dir(self.root.join(WORK_DIRECTORY))
            .env_clear()
            .envs(self.environment.iter().cloned())
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| format!("cannot start `{program}`: {error}"))?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    end_group(&mut child);
                    return Err(format!(
                        "`{program}` did not finish within {COMMAND_TIMEOUT:?}"
                    ));
                }
                Err(error) => {
                    end_group(&mut child);
                    return Err(format!("cannot wait for `{program}`: {error}"));
                }
            }
        };
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
    pub(super) fn stop(mut self) -> Result<(), String> {
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
