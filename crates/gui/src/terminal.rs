//! Stock-terminal attach: private self-deleting `.command` scripts, the
//! bounded `open` invocation, and detached child processes.
//!
//! The macOS Terminal launch never builds `AppleScript` or shell text from
//! session data. The GUI writes an owner-only script whose arguments are
//! single-quoted byte-for-byte, then asks `open -a Terminal <script>` (an argv
//! array) to run it. Terminal executes the script through its shebang, so no
//! Automation permission prompt is involved.

// Rust guideline compliant 2026-10-01
#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use pohunek_platform::filesystem::{EntryKind, FsError, StageOutcome, TrustedDir};
use rustix::process::Pid;
use serde::Deserialize;
use thiserror::Error;

/// Mode of the script directory and of every script in it: owner-only, with
/// the execute bit Terminal needs to run a `.command` file.
const PRIVATE_MODE: u32 = 0o700;

/// Directory below the pohunek runtime root that holds attach scripts.
const SCRIPT_DIR_NAME: &str = "gui-attach";

/// Name prefix of every attach script; the sweep of stale scripts matches it.
const SCRIPT_PREFIX: &str = "attach-";

/// Prefix of the temporary name a stale script is moved to before removal.
const STALE_STAGING_PREFIX: &str = ".stale-attach-";

/// Extension Terminal.app associates with "run in a new window".
const SCRIPT_EXTENSION: &str = "command";

/// Random bytes in a script name; 128 bits make the name unguessable, and the
/// exclusive create rejects any collision.
const SCRIPT_NAME_RANDOM_BYTES: usize = 16;

/// The real `open` executable; it lives at this fixed path on every macOS
/// release, so no `PATH` lookup is involved.
pub(crate) const SYSTEM_OPEN: &str = "/usr/bin/open";

/// Script interpreter line. `/bin/sh` exists on every macOS release.
const SHEBANG: &[u8] = b"#!/bin/sh\n";

/// First script command: the script removes itself before anything else runs,
/// so the file never outlives its one execution.
const SELF_DELETE: &[u8] = b"rm -f -- \"$0\"\n";

/// A stock terminal application the GUI can drive itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AttachTerminal {
    /// macOS Terminal.app, launched with `open -a Terminal <script>`.
    TerminalApp,
}

impl AttachTerminal {
    /// Value written in `gui.toml`.
    pub(crate) const fn config_name(self) -> &'static str {
        match self {
            Self::TerminalApp => "terminal-app",
        }
    }

    /// Application name passed to `open -a`.
    const fn application(self) -> &'static str {
        match self {
            Self::TerminalApp => "Terminal",
        }
    }

    /// Whether the terminal exists only on macOS.
    pub(crate) const fn requires_darwin(self) -> bool {
        match self {
            Self::TerminalApp => true,
        }
    }
}

/// Reports why an attach script cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ScriptError {
    /// A program or argument holds a NUL byte, which no process can receive.
    #[error("attach argument contains a NUL byte")]
    NulByte,
    /// The script would not execute anything.
    #[error("attach script has no program")]
    EmptyProgram,
}

/// Reports why a stock terminal could not be launched.
#[derive(Debug, Error)]
pub(crate) enum TerminalError {
    #[error(transparent)]
    Script(#[from] ScriptError),
    #[error("cannot prepare the attach script directory: {0}")]
    ScriptDirectory(#[source] FsError),
    #[error("cannot write the attach script: {0}")]
    ScriptWrite(#[source] FsError),
    #[error("cannot generate a script name: {0}")]
    Random(String),
    #[error("cannot start `{}`: {source}", opener.display())]
    OpenerSpawn { opener: PathBuf, source: io::Error },
    #[error("`{}` exited unsuccessfully ({status}){}", opener.display(), detail_suffix(.detail))]
    OpenerFailed {
        opener: PathBuf,
        status: ExitStatus,
        /// First line of the opener's standard error, sanitized.
        detail: String,
    },
    #[error("`{}` did not finish within {timeout:?} and was killed", opener.display())]
    OpenerTimeout { opener: PathBuf, timeout: Duration },
    #[error("waiting for `{}` failed: {source}", opener.display())]
    OpenerWait { opener: PathBuf, source: io::Error },
}

fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

/// Longest stderr diagnostic kept in an error, in characters.
const DETAIL_MAX_CHARS: usize = 200;

/// First non-empty line of `stderr`, lossily decoded, stripped of control
/// characters, and bounded.
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_control())
                .take(DETAIL_MAX_CHARS)
                .collect()
        })
        .unwrap_or_default()
}

/// Renders the `.command` script that runs `argv` and removes itself.
///
/// Every element is single-quoted with `'` written as `'\''`, so the shell
/// passes each value to `exec` byte-for-byte: `$()`, backticks, `;`,
/// newlines, `{placeholders}`, a leading `-`, and non-ASCII text stay data.
///
/// # Errors
///
/// Returns [`ScriptError::EmptyProgram`] for an empty `argv` or program and
/// [`ScriptError::NulByte`] when any element contains a NUL byte.
#[cfg(test)]
pub(crate) fn command_script(argv: &[OsString]) -> Result<Vec<u8>, ScriptError> {
    command_script_with_environment(&[], argv)
}

/// Variables the generated script exports for the CLI it starts: the endpoint
/// and configuration roots that `pohunek_paths` reads. Terminal.app runs the
/// script in its own environment, so without them the CLI would resolve
/// another daemon socket than the GUI's local host uses.
pub(crate) const ENDPOINT_ENVIRONMENT: [&str; 5] = [
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// Selects the [`ENDPOINT_ENVIRONMENT`] variables that are set in the GUI
/// process, in allowlist order.
///
/// `lookup` reads one variable. A relative value is skipped, as the XDG
/// specification says to ignore it; an absolute value is forwarded as the exact
/// bytes it has, UTF-8 or not, because `pohunek_paths` accepts the same values.
/// No other variable of the GUI process is ever forwarded.
pub(crate) fn endpoint_environment(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, OsString)> {
    ENDPOINT_ENVIRONMENT
        .into_iter()
        .filter_map(|name| {
            let value = lookup(name)?;
            std::path::Path::new(&value)
                .is_absolute()
                .then_some((name, value))
        })
        .collect()
}

/// [`command_script`] with `export NAME='value'` lines for `environment`
/// between the shebang and the self-delete line.
///
/// # Errors
///
/// As [`command_script`]; a NUL byte in a value is [`ScriptError::NulByte`].
pub(crate) fn command_script_with_environment(
    environment: &[(&'static str, OsString)],
    argv: &[OsString],
) -> Result<Vec<u8>, ScriptError> {
    let Some(program) = argv.first() else {
        return Err(ScriptError::EmptyProgram);
    };
    if program.is_empty() {
        return Err(ScriptError::EmptyProgram);
    }
    if argv.iter().any(|word| word.as_bytes().contains(&0))
        || environment
            .iter()
            .any(|(_, value)| value.as_bytes().contains(&0))
    {
        return Err(ScriptError::NulByte);
    }
    let mut script = Vec::new();
    script.extend_from_slice(SHEBANG);
    for (name, value) in environment {
        script.extend_from_slice(b"export ");
        script.extend_from_slice(name.as_bytes());
        script.push(b'=');
        single_quote(value.as_bytes(), &mut script);
        script.push(b'\n');
    }
    script.extend_from_slice(SELF_DELETE);
    script.extend_from_slice(b"exec");
    for word in argv {
        script.push(b' ');
        single_quote(word.as_bytes(), &mut script);
    }
    script.push(b'\n');
    Ok(script)
}

fn single_quote(word: &[u8], output: &mut Vec<u8>) {
    output.push(b'\'');
    for &byte in word {
        if byte == b'\'' {
            output.extend_from_slice(b"'\\''");
        } else {
            output.push(byte);
        }
    }
    output.push(b'\'');
}

/// Arguments after the pohunek binary that attach to `session_id` on `host`.
///
/// An empty `host` is the local machine, for which `--host` is omitted. The
/// host is one `--host=<value>` word and the id follows `--`, so neither can
/// be read as an option.
pub(crate) fn attach_arguments(host: &str, session_id: &str) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if !host.is_empty() {
        arguments.push(OsString::from(format!("--host={host}")));
    }
    arguments.push(OsString::from("attach"));
    arguments.push(OsString::from("--"));
    arguments.push(OsString::from(session_id));
    arguments
}

/// Launches sessions in a stock terminal through `open`.
#[derive(Debug, Clone)]
pub(crate) struct TerminalLauncher {
    terminal: AttachTerminal,
    opener: PathBuf,
    script_dir: PathBuf,
    open_timeout: Duration,
    script_max_age: Duration,
    environment: Vec<(&'static str, OsString)>,
}

/// What a successful launch also wants the user to know.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchReport {
    /// Stale scripts that could not be removed; the launch itself succeeded.
    pub(crate) warnings: Vec<String>,
}

impl TerminalLauncher {
    /// `opener` is `/usr/bin/open` in production and a scripted stand-in in
    /// tests; `script_dir` is an absolute path whose missing components are
    /// created owner-private.
    pub(crate) fn new(
        terminal: AttachTerminal,
        opener: PathBuf,
        script_dir: PathBuf,
        open_timeout: Duration,
        script_max_age: Duration,
    ) -> Self {
        Self {
            terminal,
            opener,
            script_dir,
            open_timeout,
            script_max_age,
            environment: Vec::new(),
        }
    }

    /// Variables the script exports before it starts the CLI; see
    /// [`endpoint_environment`].
    #[must_use]
    pub(crate) fn with_environment(mut self, environment: Vec<(&'static str, OsString)>) -> Self {
        self.environment = environment;
        self
    }

    /// Script directory below the pohunek runtime root.
    pub(crate) fn default_script_dir() -> Result<PathBuf, pohunek_paths::PathError> {
        pohunek_paths::runtime_dir().map(|root| root.join(SCRIPT_DIR_NAME))
    }

    /// Writes the script for `argv`, hands it to the opener, and waits a
    /// bounded time for the opener to exit.
    ///
    /// The script is removed again when the opener fails or is killed; on
    /// success the script deletes itself when Terminal runs it. Scripts older
    /// than the configured age that Terminal never ran are removed first; a
    /// leftover that cannot be removed is reported in the [`LaunchReport`] and
    /// never fails the launch.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] for an unbuildable script, a directory or
    /// file that is unsafe or unwritable, an opener that cannot start, exits
    /// unsuccessfully, or exceeds the timeout.
    pub(crate) fn launch(&self, argv: &[OsString]) -> Result<LaunchReport, TerminalError> {
        let contents = command_script_with_environment(&self.environment, argv)?;
        let (script, report) = self.write_script(&contents)?;
        let mut command = Command::new(&self.opener);
        command
            .arg("-a")
            .arg(self.terminal.application())
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let outcome = run_bounded(&mut command, self.open_timeout);
        let result = match outcome {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(TerminalError::OpenerFailed {
                opener: self.opener.clone(),
                status: output.status,
                detail: first_line(&output.stderr),
            }),
            Err(BoundedError::Spawn(source)) => Err(TerminalError::OpenerSpawn {
                opener: self.opener.clone(),
                source,
            }),
            Err(BoundedError::Wait(source)) => Err(TerminalError::OpenerWait {
                opener: self.opener.clone(),
                source,
            }),
            Err(BoundedError::Timeout) => Err(TerminalError::OpenerTimeout {
                opener: self.opener.clone(),
                timeout: self.open_timeout,
            }),
            Err(BoundedError::Unreaped { waited_for, budget }) => Err(TerminalError::OpenerWait {
                opener: self.opener.clone(),
                source: io::Error::other(format!(
                    "stuck after SIGKILL: waited {budget:?} for {waited_for}"
                )),
            }),
        };
        if result.is_err() {
            // The script never ran, so it removes itself nowhere else.
            let _ = std::fs::remove_file(&script);
        }
        result.map(|()| report)
    }

    /// Creates the script exclusively in the owner-private directory.
    ///
    /// `TrustedDir` validates every ancestor and the directory's owner and
    /// mode without following symlinks, and `create_file` opens with
    /// `O_CREAT | O_EXCL | O_NOFOLLOW`, so a pre-planted name or link fails
    /// instead of being reused.
    fn write_script(&self, contents: &[u8]) -> Result<(PathBuf, LaunchReport), TerminalError> {
        let directory = TrustedDir::open_or_create_absolute(&self.script_dir, PRIVATE_MODE)
            .map_err(TerminalError::ScriptDirectory)?;
        let report = LaunchReport {
            warnings: remove_stale_scripts(&directory, self.script_max_age),
        };
        let name = script_name()?;
        directory
            .create_file(&name, contents, PRIVATE_MODE)
            .map_err(TerminalError::ScriptWrite)?;
        Ok((self.script_dir.join(name), report))
    }
}

/// Whether `name` is a script this module wrote.
fn is_script_file_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.starts_with(SCRIPT_PREFIX) && name.ends_with(&format!(".{SCRIPT_EXTENSION}"))
    })
}

/// Removes attach scripts older than `max_age` that Terminal never ran.
///
/// Every step goes through the trusted directory descriptor: a candidate is
/// opened without following links (owner, mode, and link count validated), its
/// age comes from the opened inode, and the removal is bound to the inode
/// identity captured for that same file. A candidate that fails any step is
/// skipped and reported; the caller's launch never depends on this sweep.
fn remove_stale_scripts(directory: &TrustedDir, max_age: Duration) -> Vec<String> {
    let names = match directory.entry_names() {
        Ok(names) => names,
        Err(error) => return vec![format!("cannot list stale attach scripts: {error}")],
    };
    let now = SystemTime::now();
    let mut warnings = Vec::new();
    for name in names.iter().filter(|name| is_script_file_name(name)) {
        if let Err(reason) = remove_if_stale(directory, name, max_age, now) {
            warnings.push(format!(
                "cannot remove stale attach script {}: {reason}",
                name.to_string_lossy()
            ));
        }
    }
    warnings
}

fn remove_if_stale(
    directory: &TrustedDir,
    name: &OsStr,
    max_age: Duration,
    now: SystemTime,
) -> Result<(), String> {
    let Some(file) = directory
        .open_file(name, PRIVATE_MODE)
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    let modified = metadata.modified().map_err(|error| error.to_string())?;
    // A clock that moved backwards makes the file look new; it is kept.
    let stale = now.duration_since(modified).is_ok_and(|age| age >= max_age);
    if !stale {
        return Ok(());
    }
    let identity = directory
        .entry_identity(name, EntryKind::RegularFile)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the script disappeared".to_owned())?;
    if identity.device() != metadata.dev() || identity.inode() != metadata.ino() {
        return Err("the script was replaced while it was inspected".to_owned());
    }
    match directory
        .stage_random(name, STALE_STAGING_PREFIX, identity)
        .map_err(|error| error.to_string())?
    {
        StageOutcome::Staged(entry) => entry
            .remove()
            .map(|_| ())
            .map_err(|error| error.to_string()),
        StageOutcome::Missing => Ok(()),
        StageOutcome::DestinationExists | StageOutcome::IdentityChanged => {
            Err("the script changed while it was removed".to_owned())
        }
        other => Err(format!("unexpected staging outcome: {other:?}")),
    }
}

fn script_name() -> Result<OsString, TerminalError> {
    let mut random = [0_u8; SCRIPT_NAME_RANDOM_BYTES];
    getrandom::getrandom(&mut random).map_err(|error| TerminalError::Random(error.to_string()))?;
    let mut name = String::from(SCRIPT_PREFIX);
    for byte in random {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("writing hexadecimal to a String cannot fail");
    }
    name.push('.');
    name.push_str(SCRIPT_EXTENSION);
    Ok(name.into())
}

/// Reports why a bounded run produced no exit status.
#[derive(Debug)]
pub(crate) enum BoundedError {
    Spawn(io::Error),
    Wait(io::Error),
    /// The deadline passed; the group was killed and the leader reaped.
    Timeout,
    /// The group was killed but the leader still could not be reaped within
    /// the reap budget (it is stuck in the kernel); the process was abandoned.
    Unreaped {
        /// What was waited for.
        waited_for: &'static str,
        /// How long it was waited.
        budget: Duration,
    },
}

/// Exit status of a bounded run and any captured standard error.
#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: ExitStatus,
    /// At most [`STDERR_CAPTURE_LIMIT`] bytes, empty unless the command piped stderr.
    pub(crate) stderr: Vec<u8>,
}

/// Most standard-error bytes kept from a bounded run; a diagnostic needs only
/// a short first line.
const STDERR_CAPTURE_LIMIT: u64 = 4096;

/// Pause between exit checks of a bounded run. A check is one `wait4(WNOHANG)`,
/// so 10 ms bounds both the added latency of a quick command and the cost of
/// the polling thread.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Longest wait for the leader to be reaped after the group received SIGKILL.
/// SIGKILL ends a running or stopped process at once; five seconds only
/// matters for a process stuck in the kernel, which is then reported instead
/// of waited for.
const REAP_BUDGET: Duration = Duration::from_secs(5);

/// Longest wait for the standard-error reader after the group is dead. The
/// pipe reaches EOF once every holder is gone; a holder outside the group
/// (one that called `setsid`) can keep it open, and its output is then dropped.
const STDERR_DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Runs `command` in its own process group and waits at most `timeout` for it
/// to exit.
///
/// The leader's exit is polled with `try_wait`, never with a blocking wait or
/// `waitid`, so no step depends on how a platform reports a stopped or killed
/// process. Only the deadline signals anything: the whole group gets SIGKILL
/// while the leader is still unreaped (it is running or a zombie, and either
/// way its pid, the group id, cannot be recycled), and the leader is then
/// reaped within [`REAP_BUDGET`]. A leader that `try_wait` reports as exited is
/// already reaped, so nothing is signalled after a normal exit: the group id of
/// a reaped leader may belong to an unrelated group by then. A wrapper that
/// backgrounds a child and exits has detached it on purpose.
///
/// Standard error, when piped, is drained by a thread from the start (a full
/// pipe cannot stall the command) and joined within [`STDERR_DRAIN_BUDGET`]; a
/// surviving descendant that keeps the pipe open costs that budget and the
/// diagnostic.
///
/// The limit: a descendant that leaves the group (`setsid` or `setpgid`) is
/// out of reach.
pub(crate) fn run_bounded(
    command: &mut Command,
    timeout: Duration,
) -> Result<BoundedOutput, BoundedError> {
    let mut child = command
        .process_group(0)
        .spawn()
        .map_err(BoundedError::Spawn)?;
    let group = Pid::from_child(&child);
    let stderr_reader = child.stderr.take().map(drain_stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(source) => {
                kill_group(group);
                reap_within(&mut child, REAP_BUDGET)?;
                return Err(BoundedError::Wait(source));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            kill_group(group);
            reap_within(&mut child, REAP_BUDGET)?;
            return Err(BoundedError::Timeout);
        }
        thread::sleep(remaining.min(EXIT_POLL_INTERVAL));
    };
    let stderr = stderr_reader
        .and_then(|receiver| receiver.recv_timeout(STDERR_DRAIN_BUDGET).ok())
        .unwrap_or_default();
    Ok(BoundedOutput { status, stderr })
}

/// Reads `pipe` to EOF on a thread, keeping the first [`STDERR_CAPTURE_LIMIT`]
/// bytes and discarding the rest.
///
/// The pipe stays open until EOF, so a writer of any size never gets SIGPIPE.
fn drain_stderr(mut pipe: std::process::ChildStderr) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("pohunek-gui-stderr".to_owned())
        .spawn(move || {
            let limit = usize::try_from(STDERR_CAPTURE_LIMIT).unwrap_or(usize::MAX);
            let mut kept = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        let room = limit.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..count.min(room)]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    // A read error only loses the diagnostic.
                    Err(_) => break,
                }
            }
            let _ = sender.send(kept);
        });
    // Without the thread the sender is dropped and the receiver reports
    // disconnection, which the caller treats as no diagnostic.
    drop(spawned);
    receiver
}

/// Reaps the killed leader, polling for at most `budget`.
fn reap_within(child: &mut std::process::Child, budget: Duration) -> Result<(), BoundedError> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(source) => return Err(BoundedError::Wait(source)),
        }
        if Instant::now() >= deadline {
            return Err(BoundedError::Unreaped {
                waited_for: "the killed process to exit",
                budget,
            });
        }
        thread::sleep(EXIT_POLL_INTERVAL);
    }
}

/// Kills the process group `group`.
///
/// An already gone group (`ESRCH`) is fine.
fn kill_group(group: Pid) {
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
}

/// Why an observed child did not count as started.
#[derive(Debug, Error)]
pub(crate) enum ObserveError {
    #[error("cannot start `{}`: {source}", program.to_string_lossy())]
    Spawn {
        program: OsString,
        source: io::Error,
    },
    #[error("`{}` exited unsuccessfully within the observation window ({status})", program.to_string_lossy())]
    ExitedEarly {
        program: OsString,
        status: ExitStatus,
    },
    #[error("waiting for `{}` failed: {source}", program.to_string_lossy())]
    Wait {
        program: OsString,
        source: io::Error,
    },
}

/// Starts `program` detached from the GUI, reaps it, and reports a quick failure.
///
/// The child gets its own process group and null stdio, so it survives GUI
/// exit and terminal signals aimed at the GUI's group. It is never killed on
/// drop. A reaper thread owns the `wait`, so no zombie outlives the process,
/// and sends the exit status back: a non-zero or signal exit within `window`
/// is an error (a dead template, a missing terminal), while a child still
/// running at the deadline or one that exited 0 counts as started. The reaper
/// keeps waiting after the window closes.
///
/// The reaper thread exists before the process does. If the process cannot be
/// handed to it, the process is killed and reaped here, so no child is left
/// unreaped and no success follows an error.
///
/// # Errors
///
/// Returns [`ObserveError`] for a spawn failure or an early unsuccessful exit.
pub(crate) fn spawn_observed(
    program: &OsStr,
    arguments: &[OsString],
    window: Duration,
) -> Result<(), ObserveError> {
    let (child_sender, child_receiver) = mpsc::channel::<std::process::Child>();
    let (status_sender, status_receiver) = mpsc::channel();
    thread::Builder::new()
        .name("pohunek-gui-reaper".to_owned())
        .spawn(move || {
            if let Ok(mut child) = child_receiver.recv() {
                let _ = status_sender.send(child.wait());
            }
        })
        .map_err(|source| ObserveError::Spawn {
            program: program.to_owned(),
            source,
        })?;
    let child = Command::new(program)
        .args(arguments)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| ObserveError::Spawn {
            program: program.to_owned(),
            source,
        })?;
    if let Err(mpsc::SendError(mut orphan)) = child_sender.send(child) {
        let _ = orphan.kill();
        let _ = orphan.wait();
        return Err(ObserveError::Spawn {
            program: program.to_owned(),
            source: io::Error::other("the reaper thread ended before taking the process"),
        });
    }
    match status_receiver.recv_timeout(window) {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(ObserveError::ExitedEarly {
            program: program.to_owned(),
            status,
        }),
        Ok(Err(source)) => Err(ObserveError::Wait {
            program: program.to_owned(),
            source,
        }),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(()),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ObserveError::Wait {
            program: program.to_owned(),
            source: io::Error::other("the reaper thread ended without an exit status"),
        }),
    }
}

/// Whether `path` names a script this module wrote.
#[cfg(test)]
fn is_script_name(path: &std::path::Path) -> bool {
    path.file_name().is_some_and(is_script_file_name)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::*;

    /// Values that break naive quoting.
    fn difficult_values() -> Vec<String> {
        [
            "plain",
            "with space",
            "it's",
            "'",
            "''",
            "say \"hi\"",
            "$(echo INJECTED)",
            "`echo INJECTED`",
            "a;echo INJECTED",
            "a&&echo INJECTED",
            "a|echo INJECTED",
            "line one\nline two",
            "trailing newline\n",
            "tab\there",
            "back\\slash",
            "\u{10d}esk\u{fd} projekt \u{65e5}\u{672c}\u{8a9e} \u{1f980}",
            "{bin}",
            "{host}",
            "{id}",
            "{host};echo INJECTED",
            "$HOME ~ * ?",
            "--flag=value",
            "-rf",
            "-",
            "--",
            "",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// Writes an executable stand-in that prints its argv NUL-terminated.
    fn recorder(dir: &Path) -> PathBuf {
        let path = dir.join("recorder");
        fs::write(&path, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").expect("write recorder");
        crate::test_support::make_executable(&path);
        path
    }

    fn nul_split(output: &[u8]) -> Vec<Vec<u8>> {
        let mut words: Vec<Vec<u8>> = output
            .split(|byte| *byte == 0)
            .map(<[u8]>::to_vec)
            .collect();
        assert_eq!(words.pop(), Some(Vec::new()), "output ends with a NUL");
        words
    }

    fn run_script(script_path: &Path) -> Vec<Vec<u8>> {
        let output = Command::new("sh")
            .arg(script_path)
            .output()
            .expect("run script");
        assert!(output.status.success(), "script exits successfully");
        assert!(output.stderr.is_empty(), "script writes nothing to stderr");
        nul_split(&output.stdout)
    }

    #[test]
    fn script_delivers_every_difficult_value_byte_exact() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = recorder(dir.path());
        let values = difficult_values();
        let mut argv = vec![recorder.into_os_string()];
        argv.extend(values.iter().map(OsString::from));
        let script = dir.path().join("script.command");
        fs::write(&script, command_script(&argv).expect("script")).expect("write script");

        let received = run_script(&script);

        let expected: Vec<Vec<u8>> = values
            .iter()
            .map(|value| value.as_bytes().to_vec())
            .collect();
        assert_eq!(received, expected);
    }

    #[test]
    fn script_delivers_a_non_utf8_argument_byte_exact() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = recorder(dir.path());
        let raw = OsStr::from_bytes(b"caf\xe9 '\xff'").to_owned();
        let argv = vec![recorder.into_os_string(), raw.clone()];
        let script = dir.path().join("script.command");
        fs::write(&script, command_script(&argv).expect("script")).expect("write script");

        assert_eq!(run_script(&script), vec![raw.as_bytes().to_vec()]);
    }

    #[test]
    fn script_deletes_itself_before_running_the_program() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let marker = dir.path().join("script-existed");
        let probe = dir.path().join("probe");
        // The probe records whether the script file was already gone.
        fs::write(
            &probe,
            format!(
                "#!/bin/sh\nif [ -e \"$1\" ]; then echo present > '{marker}'; else echo gone > '{marker}'; fi\n",
                marker = marker.display()
            ),
        )
        .expect("write probe");
        crate::test_support::make_executable(&probe);
        let script = dir.path().join("script.command");
        let argv = vec![probe.into_os_string(), script.clone().into_os_string()];
        fs::write(&script, command_script(&argv).expect("script")).expect("write script");

        let status = Command::new("sh").arg(&script).status().expect("run");

        assert!(status.success());
        assert!(!script.exists(), "script removed itself");
        assert_eq!(fs::read_to_string(marker).expect("marker").trim(), "gone");
    }

    #[test]
    fn endpoint_environment_forwards_only_the_allowlist_of_absolute_values() {
        let _watchdog = crate::test_support::watchdog();
        let host: Vec<(&str, OsString)> = vec![
            ("XDG_RUNTIME_DIR", OsString::from("/run/user/1000")),
            ("XDG_CONFIG_HOME", OsString::from("relative/config")),
            ("XDG_DATA_HOME", OsStr::from_bytes(b"/data/\xff").to_owned()),
            ("XDG_STATE_HOME", OsString::from("/state")),
            ("AWS_SECRET_ACCESS_KEY", OsString::from("/not-forwarded")),
            ("HOME", OsString::from("/home/x")),
            ("PATH", OsString::from("/usr/bin")),
        ];

        let forwarded = endpoint_environment(|name| {
            host.iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| value.clone())
        });

        assert_eq!(
            forwarded,
            vec![
                ("XDG_RUNTIME_DIR", OsString::from("/run/user/1000")),
                ("XDG_DATA_HOME", OsStr::from_bytes(b"/data/\xff").to_owned()),
                ("XDG_STATE_HOME", OsString::from("/state")),
            ]
        );
    }

    #[test]
    fn the_script_exports_the_allowlist_byte_exact_and_nothing_else() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = dir.path().join("recorder");
        // Prints every exported variable name, then each allowlisted value,
        // NUL-terminated; `env` lists the names the program really received.
        fs::write(
            &recorder,
            r#"#!/bin/sh
env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' | sort
printf '==\n'
for n in XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME; do
  eval "v=\${$n-UNSET}"
  printf '%s\0' "$v"
done
"#,
        )
        .expect("write recorder");
        crate::test_support::make_executable(&recorder);
        let hostile = [
            "/run/it's \"quoted\"\nnext line",
            "/cfg/$(echo INJECTED)`id`;x",
            "/data/\u{10d}esk\u{fd} \u{1f980}",
            "/state/{bin}",
            "/cache/--flag",
        ];
        let environment: Vec<(&'static str, OsString)> = ENDPOINT_ENVIRONMENT
            .into_iter()
            .zip(hostile)
            .map(|(name, value)| (name, OsString::from(value)))
            .collect();
        let script = dir.path().join("script.command");
        fs::write(
            &script,
            command_script_with_environment(&environment, &[recorder.into_os_string()])
                .expect("script"),
        )
        .expect("write script");

        let output = Command::new("sh")
            .arg(&script)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH"))
            .output()
            .expect("run script");

        assert!(output.status.success());
        let text = output.stdout;
        let split = text
            .windows(3)
            .position(|window| window == b"==\n")
            .expect("separator");
        let names = String::from_utf8_lossy(&text[..split]).into_owned();
        // The shell may add its own bookkeeping variables; nothing else.
        for name in names.lines() {
            assert!(
                ENDPOINT_ENVIRONMENT.contains(&name)
                    || ["PATH", "PWD", "OLDPWD", "SHLVL", "_", "LC_ALL", "LANG"].contains(&name),
                "unexpected variable {name}"
            );
        }
        let values: Vec<&[u8]> = text[split + 3..]
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .collect();
        let expected: Vec<&[u8]> = hostile.iter().map(|value| value.as_bytes()).collect();
        assert_eq!(values, expected);
    }

    #[test]
    fn a_launch_exports_the_endpoint_environment_to_the_cli() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let output = dir.path().join("seen-runtime-dir");
        let opener = fake_open(dir.path(), &record, "/bin/sh \"$3\"");
        let target = dir.path().join("target");
        fs::write(
            &target,
            format!(
                "#!/bin/sh\nprintf '%s' \"$XDG_RUNTIME_DIR\" > '{}'\n",
                output.display()
            ),
        )
        .expect("write target");
        crate::test_support::make_executable(&target);
        let launcher = launcher(opener, dir.path().join("gui-attach"), WINDOW).with_environment(
            vec![("XDG_RUNTIME_DIR", OsString::from("/run/pohunek-test"))],
        );

        launcher.launch(&[target.into_os_string()]).expect("launch");

        assert_eq!(
            fs::read_to_string(&output).expect("recorded"),
            "/run/pohunek-test"
        );
    }

    #[test]
    fn a_non_utf8_value_is_exported_byte_exact() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = dir.path().join("recorder");
        fs::write(
            &recorder,
            "#!/bin/sh\nprintf '%s' \"$XDG_RUNTIME_DIR\" > \"$0.out\"\n",
        )
        .expect("write recorder");
        crate::test_support::make_executable(&recorder);
        let raw = OsStr::from_bytes(b"/run/caf\xe9 '\xff'").to_owned();
        let script = dir.path().join("script.command");
        fs::write(
            &script,
            command_script_with_environment(
                &[("XDG_RUNTIME_DIR", raw.clone())],
                &[recorder.clone().into_os_string()],
            )
            .expect("script"),
        )
        .expect("write script");

        let status = Command::new("sh").arg(&script).status().expect("run");

        assert!(status.success());
        let seen = fs::read(dir.path().join("recorder.out")).expect("recorded");
        assert_eq!(seen, raw.as_bytes());
    }

    #[test]
    fn a_nul_byte_in_an_exported_value_is_rejected() {
        let _watchdog = crate::test_support::watchdog();
        assert_eq!(
            command_script_with_environment(
                &[("XDG_RUNTIME_DIR", OsString::from("/a\0b"))],
                &[OsString::from("/usr/bin/true")]
            ),
            Err(ScriptError::NulByte)
        );
    }

    #[test]
    fn a_stderr_writer_larger_than_the_capture_limit_is_not_cut_off() {
        let _watchdog = crate::test_support::watchdog();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "head -c 65536 /dev/zero >&2; printf 'tail' >&2"])
            .stderr(Stdio::piped());

        let output = run_bounded(&mut command, WINDOW).expect("writer finishes");

        // A closed pipe would end the writer with SIGPIPE instead of exit 0.
        assert_eq!(output.status.code(), Some(0), "{:?}", output.status);
        assert_eq!(output.stderr.len(), 4096);
    }

    #[test]
    fn script_layout_is_shebang_self_delete_exec() {
        let _watchdog = crate::test_support::watchdog();
        let script =
            command_script(&[OsString::from("/bin/echo"), OsString::from("it's")]).expect("script");
        assert_eq!(
            String::from_utf8(script).expect("utf8"),
            "#!/bin/sh\nrm -f -- \"$0\"\nexec '/bin/echo' 'it'\\''s'\n"
        );
    }

    #[test]
    fn nul_bytes_and_empty_programs_are_rejected() {
        let _watchdog = crate::test_support::watchdog();
        assert_eq!(
            command_script(&[OsString::from("/bin/echo"), OsString::from("a\0b")]),
            Err(ScriptError::NulByte)
        );
        assert_eq!(
            command_script(&[OsString::from("/bin/e\0cho")]),
            Err(ScriptError::NulByte)
        );
        assert_eq!(command_script(&[]), Err(ScriptError::EmptyProgram));
        assert_eq!(
            command_script(&[OsString::new()]),
            Err(ScriptError::EmptyProgram)
        );
    }

    #[test]
    fn attach_arguments_keep_host_and_id_out_of_option_position() {
        let _watchdog = crate::test_support::watchdog();
        assert_eq!(
            attach_arguments("", "s-1"),
            ["attach", "--", "s-1"].map(OsString::from)
        );
        assert_eq!(
            attach_arguments("--evil host", "-x"),
            ["--host=--evil host", "attach", "--", "-x"].map(OsString::from)
        );
    }

    /// Writes a fake `open` that records its argv and runs the script it was
    /// given, like Terminal would.
    fn fake_open(dir: &Path, record: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-open");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{record}'\n{body}\n",
                record = record.display()
            ),
        )
        .expect("write fake open");
        crate::test_support::make_executable(&path);
        path
    }

    /// Scripts older than this are stale in the tests.
    const TEST_MAX_AGE: Duration = Duration::from_secs(3600);

    fn launcher(opener: PathBuf, script_dir: PathBuf, timeout: Duration) -> TerminalLauncher {
        TerminalLauncher::new(
            AttachTerminal::TerminalApp,
            opener,
            script_dir,
            timeout,
            TEST_MAX_AGE,
        )
    }

    fn scripts_in(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .expect("read script dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| is_script_name(path))
            .collect()
    }

    #[test]
    fn launch_invokes_open_with_an_argv_array_and_runs_the_script() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let output = dir.path().join("attach-argv");
        // The fake terminal runs the script it was handed, then records nothing else.
        let opener = fake_open(dir.path(), &record, "/bin/sh \"$3\"");
        let target = dir.path().join("target");
        fs::write(
            &target,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\n",
                output.display()
            ),
        )
        .expect("write target");
        crate::test_support::make_executable(&target);
        let script_dir = dir.path().join("private").join("gui-attach");
        let launcher = launcher(opener.clone(), script_dir.clone(), Duration::from_secs(30));
        let mut argv = vec![target.into_os_string()];
        argv.extend(attach_arguments("h$(x) 'y'", "-id; echo INJECTED"));

        launcher.launch(&argv).expect("launch");

        let open_args = nul_split(&fs::read(&record).expect("open argv"));
        assert_eq!(open_args.len(), 3);
        assert_eq!(open_args[0], b"-a");
        assert_eq!(open_args[1], b"Terminal");
        let script_path = PathBuf::from(OsStr::from_bytes(&open_args[2]));
        assert!(script_path.starts_with(&script_dir));
        assert!(is_script_name(&script_path));
        assert_eq!(
            nul_split(&fs::read(&output).expect("attach argv")),
            vec![
                b"--host=h$(x) 'y'".to_vec(),
                b"attach".to_vec(),
                b"--".to_vec(),
                b"-id; echo INJECTED".to_vec(),
            ]
        );
        assert!(!script_path.exists(), "script removed itself after running");
    }

    #[test]
    fn script_directory_and_file_are_owner_private() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        // Leaves the script in place so its mode can be inspected.
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect("launch");

        let directory_mode = fs::metadata(&script_dir).expect("dir").permissions().mode();
        assert_eq!(directory_mode & 0o777, 0o700);
        let scripts = scripts_in(&script_dir);
        assert_eq!(scripts.len(), 1);
        let file_mode = fs::metadata(&scripts[0])
            .expect("file")
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o700);
    }

    #[test]
    fn a_missing_runtime_hierarchy_is_created_owner_private() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        // The runtime directory and its parent do not exist yet, like a first
        // launch under /private/tmp/pohunek-<uid>.
        let runtime = dir.path().join("pohunek-1000");
        let script_dir = runtime.join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), WINDOW);

        launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect("launch creates the hierarchy");

        for created in [&runtime, &script_dir] {
            let mode = fs::metadata(created)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{}", created.display());
        }
    }

    #[test]
    fn every_launch_uses_a_fresh_script_name() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect("first");
        launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect("second");

        assert_eq!(scripts_in(&script_dir).len(), 2);
    }

    #[test]
    fn a_group_writable_script_directory_is_refused() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        fs::create_dir(&script_dir).expect("mkdir");
        fs::set_permissions(&script_dir, fs::Permissions::from_mode(0o770)).expect("chmod");
        let launcher = launcher(opener, script_dir, Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect_err("unsafe directory");

        assert!(
            matches!(error, TerminalError::ScriptDirectory(_)),
            "{error}"
        );
        assert!(!record.exists(), "opener never ran");
    }

    #[test]
    fn a_symlinked_script_directory_is_refused() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("mkdir");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("chmod");
        let link = dir.path().join("gui-attach");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let launcher = launcher(opener, link, Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect_err("symlinked directory");

        assert!(
            matches!(error, TerminalError::ScriptDirectory(_)),
            "{error}"
        );
        assert!(fs::read_dir(&real).expect("real dir").next().is_none());
    }

    #[test]
    fn a_nul_byte_fails_before_any_file_or_process_exists() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true"), OsString::from("a\0b")])
            .expect_err("nul");

        assert!(matches!(error, TerminalError::Script(ScriptError::NulByte)));
        assert!(!script_dir.exists());
        assert!(!record.exists());
    }

    #[test]
    fn a_failing_opener_is_a_typed_error_and_leaves_no_script() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 3");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect_err("failing opener");

        assert!(
            matches!(error, TerminalError::OpenerFailed { .. }),
            "{error}"
        );
        assert!(scripts_in(&script_dir).is_empty());
    }

    #[test]
    fn a_missing_opener_is_a_typed_error_and_leaves_no_script() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(
            dir.path().join("no-such-open"),
            script_dir.clone(),
            Duration::from_secs(30),
        );

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect_err("missing opener");

        assert!(
            matches!(error, TerminalError::OpenerSpawn { .. }),
            "{error}"
        );
        assert!(scripts_in(&script_dir).is_empty());
    }

    #[test]
    fn a_stalled_opener_is_killed_at_the_deadline() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        // A stopped process never exits by itself; only SIGKILL ends it.
        let opener = fake_open(dir.path(), &record, "kill -STOP $$");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_millis(200));

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect_err("stalled opener");

        assert!(
            matches!(error, TerminalError::OpenerTimeout { .. }),
            "{error}"
        );
        assert!(scripts_in(&script_dir).is_empty());
    }

    const WINDOW: Duration = Duration::from_secs(30);

    /// Whether `pid` still runs; a zombie awaiting its reaper counts as gone.
    fn process_is_running(pid: rustix::process::Pid) -> bool {
        let stat = format!("/proc/{}/stat", pid.as_raw_nonzero());
        match fs::read_to_string(stat) {
            Ok(text) => text
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.trim_start().chars().next())
                .is_some_and(|state| state != 'Z'),
            Err(_) if Path::new("/proc/self").exists() => false,
            Err(_) => rustix::process::test_kill_process(pid).is_ok(),
        }
    }

    /// Waits a bounded time for the pid recorded in `marker` to disappear.
    fn assert_background_child_gone(marker: &Path) {
        let pid: i32 = fs::read_to_string(marker)
            .expect("marker")
            .trim()
            .parse()
            .expect("pid");
        let pid = rustix::process::Pid::from_raw(pid).expect("pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while process_is_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "a descendant survived the group kill"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_timeout_kills_the_wrappers_background_children_too() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let marker = dir.path().join("background-pid");
        let wrapper = dir.path().join("wrapper");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nsleep 300 &\necho $! > '{}'\nwait\n",
                marker.display()
            ),
        )
        .expect("write wrapper");
        crate::test_support::make_executable(&wrapper);

        let error = run_bounded(&mut Command::new(&wrapper), Duration::from_secs(2))
            .expect_err("the wrapper never exits");

        assert!(matches!(error, BoundedError::Timeout), "{error:?}");
        assert_background_child_gone(&marker);
    }

    #[test]
    fn a_wrapper_that_exits_normally_does_not_get_its_group_signalled() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let marker = dir.path().join("background-pid");
        let wrapper = dir.path().join("wrapper");
        // The child detaches its stdio on purpose; after a normal exit nothing
        // may signal the group, whose id may no longer be ours.
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nsleep 300 >/dev/null 2>&1 </dev/null &\necho $! > '{}'\nexit 0\n",
                marker.display()
            ),
        )
        .expect("write wrapper");
        crate::test_support::make_executable(&wrapper);

        let output = run_bounded(&mut Command::new(&wrapper), WINDOW).expect("wrapper exits");

        assert!(output.status.success());
        let pid: i32 = fs::read_to_string(&marker)
            .expect("marker")
            .trim()
            .parse()
            .expect("pid");
        let pid = rustix::process::Pid::from_raw(pid).expect("pid");
        let survived = process_is_running(pid);
        // Stop the survivor before asserting, so a failure leaves nothing behind.
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        assert!(
            survived,
            "the detached child was signalled after a normal exit"
        );
    }

    #[test]
    fn a_quick_successful_exit_counts_as_started() {
        let _watchdog = crate::test_support::watchdog();
        spawn_observed(
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("exit 0")],
            WINDOW,
        )
        .expect("started");
    }

    #[test]
    fn a_quick_failing_exit_is_reported_with_its_status() {
        let _watchdog = crate::test_support::watchdog();
        let error = spawn_observed(
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("exit 127")],
            WINDOW,
        )
        .expect_err("dead template");

        assert!(
            matches!(&error, ObserveError::ExitedEarly { status, .. } if status.code() == Some(127)),
            "{error}"
        );
    }

    #[test]
    fn a_signalled_child_is_reported() {
        let _watchdog = crate::test_support::watchdog();
        let error = spawn_observed(
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("kill -KILL $$")],
            WINDOW,
        )
        .expect_err("killed child");

        assert!(matches!(error, ObserveError::ExitedEarly { .. }), "{error}");
    }

    #[test]
    fn a_child_still_running_at_the_deadline_counts_as_started_and_is_reaped_later() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let fifo = dir.path().join("release.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(status.success());

        // The child blocks reading the FIFO until the test releases it.
        spawn_observed(
            OsStr::new("/bin/sh"),
            &[
                OsString::from("-c"),
                OsString::from(format!("read line < '{}'", fifo.display())),
            ],
            Duration::from_millis(100),
        )
        .expect("long-lived terminal");

        fs::write(&fifo, "go\n").expect("release the child");
    }

    #[test]
    fn spawn_observed_reports_a_missing_program() {
        let _watchdog = crate::test_support::watchdog();
        let error = spawn_observed(OsStr::new("/nonexistent/pohunek-attach"), &[], WINDOW)
            .expect_err("missing program");

        assert!(
            matches!(&error, ObserveError::Spawn { source, .. } if source.kind() == io::ErrorKind::NotFound),
            "{error}"
        );
    }

    #[test]
    fn spawn_observed_puts_the_child_in_its_own_process_group() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let output = dir.path().join("pgid");
        spawn_observed(
            OsStr::new("/bin/sh"),
            &[
                OsString::from("-c"),
                OsString::from(format!(
                    "ps -o pgid= -p $$ > '{}'; ps -o pid= -p $$ >> '{}'",
                    output.display(),
                    output.display()
                )),
            ],
            WINDOW,
        )
        .expect("spawn");

        let text = fs::read_to_string(&output).expect("ps output");
        let mut numbers = text.split_whitespace();
        let group = numbers.next().expect("pgid");
        let process = numbers.next().expect("pid");
        assert_eq!(group, process, "child leads its own process group");
    }

    #[test]
    fn an_opener_failure_carries_the_first_stderr_line() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(
            dir.path(),
            &record,
            "printf '\\n  Unable to find application named Terminal\\nsecond line\\n' >&2\nexit 1",
        );
        let launcher = launcher(opener, dir.path().join("gui-attach"), WINDOW);

        let error = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect_err("failing opener");

        let message = error.to_string();
        assert!(
            message.contains("Unable to find application named Terminal"),
            "{message}"
        );
        assert!(!message.contains("second line"), "{message}");
    }

    #[test]
    fn first_line_strips_control_characters_and_bounds_length() {
        let _watchdog = crate::test_support::watchdog();
        assert_eq!(first_line(b"\n\x1b[31mred\x07\nnext"), "[31mred");
        assert_eq!(first_line(b"   \n"), "");
        assert_eq!(first_line(&[b'x'; 1000]).chars().count(), DETAIL_MAX_CHARS);
        assert_eq!(first_line(b"caf\xe9").chars().count(), 4);
    }

    fn age(path: &Path, by: Duration) {
        let file = fs::OpenOptions::new().write(true).open(path).expect("open");
        file.set_modified(SystemTime::now() - by)
            .expect("set mtime");
    }

    fn private_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        path
    }

    #[test]
    fn a_launch_removes_stale_scripts_and_keeps_fresh_and_unrelated_files() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        fs::create_dir(&script_dir).expect("mkdir");
        fs::set_permissions(&script_dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        let old = private_file(&script_dir, "attach-old.command");
        let fresh = private_file(&script_dir, "attach-fresh.command");
        let unrelated = private_file(&script_dir, "keep-me.txt");
        age(&old, Duration::from_secs(7200));
        age(&unrelated, Duration::from_secs(7200));
        let launcher = launcher(opener, script_dir.clone(), WINDOW);

        let report = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect("launch");

        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(!old.exists(), "stale script removed");
        assert!(fresh.exists(), "fresh script kept");
        assert!(unrelated.exists(), "unrelated file kept");
    }

    #[test]
    fn an_unremovable_stale_script_is_a_warning_and_never_fails_the_launch() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        fs::create_dir(&script_dir).expect("mkdir");
        fs::set_permissions(&script_dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        // A symlink with a script name is never followed or removed.
        let target = dir.path().join("target");
        fs::write(&target, "x").expect("target");
        let link = script_dir.join("attach-link.command");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        // A script with the wrong mode fails validation.
        let loose = private_file(&script_dir, "attach-loose.command");
        crate::test_support::make_executable(&loose);
        let launcher = launcher(opener, script_dir, WINDOW);

        let report = launcher
            .launch(&[OsString::from("/usr/bin/true")])
            .expect("launch still succeeds");

        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
        assert!(link.symlink_metadata().is_ok(), "symlink untouched");
        assert!(target.exists(), "symlink target untouched");
        assert!(loose.exists());
    }
}
