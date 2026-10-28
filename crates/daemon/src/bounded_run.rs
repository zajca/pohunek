//! Bounded one-shot process runner.
//!
//! Runs one fixed program to completion under hard limits. The caller supplies
//! the program, its literal arguments and any fixed environment pairs; nothing
//! else reaches the child. The child gets a closed stdin, a cleared
//! environment plus a private scratch sandbox, its own process group and a
//! wall-clock deadline. Its stdout is drained while it runs and capped, so a
//! flood ends the run at once; its stderr is dropped or kept only as a short
//! tail for the daemon's local log. The group is killed on deadline, overflow,
//! cancellation and after the child exits, and the child is always reaped.
//!
//! The child is spawned without `pre_exec` so std can use `posix_spawn`, whose
//! cost does not grow with the daemon's address space.

// Rust guideline compliant 2026-10-26

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::debug;

/// Largest stdout a run may retain; a larger caller cap is lowered to it.
pub(crate) const MAX_STDOUT_CAP_BYTES: usize = 128 * 1024;
/// Most stderr bytes kept for the local log, newest last.
const STDERR_TAIL_BYTES: usize = 4 * 1024;
/// Poll cadence balances bounded shutdown with negligible idle CPU use.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// A deterministic locale keeps program output stable.
const RUN_LOCALE: &str = "C";
/// Bytes read from a pipe per call.
const READ_CHUNK_BYTES: usize = 8 * 1024;
/// Most bytes one drain call takes from a pipe that retains its last bytes, so
/// a writer that never pauses cannot hold the supervising loop.
const TAIL_DRAIN_BUDGET_BYTES: usize = 64 * 1024;
/// The environment variable that holds the executable search path.
const SEARCH_PATH_VAR: &str = "PATH";
/// Prefix of the per-run sandbox directory under the temp dir.
const SANDBOX_DIR_PREFIX: &str = "pohunek-run-";
/// Attempts to find an unused sandbox directory name.
const SANDBOX_CREATE_ATTEMPTS: usize = 4;

/// Wall-clock and output limits of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunLimits {
    timeout: Duration,
    stdout_cap: usize,
}

impl RunLimits {
    /// Limits that end the run after `timeout` or once stdout exceeds
    /// `stdout_cap` bytes, which [`MAX_STDOUT_CAP_BYTES`] bounds.
    #[must_use]
    pub(crate) fn new(timeout: Duration, stdout_cap: usize) -> Self {
        Self {
            timeout,
            stdout_cap: stdout_cap.min(MAX_STDOUT_CAP_BYTES),
        }
    }
}

/// What a run does with the child's stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StderrPolicy {
    /// Stderr goes to the null device.
    Discard,
    /// The last [`STDERR_TAIL_BYTES`] bytes go to the daemon's debug log and
    /// the outcome; they are never part of a reply.
    #[expect(dead_code, reason = "first used by the package command runner")]
    LogTail,
}

/// One run: the fixed program, arguments and environment, and the limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunSpec<'a> {
    /// The program to execute.
    pub(crate) program: &'a Path,
    /// Arguments passed literally.
    pub(crate) args: &'a [String],
    /// The `PATH` the child sees, or none.
    pub(crate) search_path: Option<&'a OsStr>,
    /// Fixed pairs set right after the environment is cleared. They never
    /// name a variable the sandbox sets.
    pub(crate) env: &'a [(String, String)],
    /// Variables set before the clear, so a caller can show that the ambient
    /// environment does not reach the child.
    pub(crate) seeded_env: &'a [(&'a str, &'a str)],
    /// The run's deadline and stdout cap.
    pub(crate) limits: RunLimits,
    /// What happens to stderr.
    pub(crate) stderr: StderrPolicy,
}

/// The result of a run whose child exited on its own.
#[derive(Debug)]
pub(crate) struct RunOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    #[expect(dead_code, reason = "first used by the package command runner")]
    stderr_tail: Vec<u8>,
}

impl RunOutput {
    /// How the child exited.
    #[must_use]
    pub(crate) fn status(&self) -> ExitStatus {
        self.status
    }

    /// The retained stdout bytes, at most the run's cap.
    #[must_use]
    #[expect(dead_code, reason = "first used by the package command runner")]
    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// The number of retained stdout bytes.
    #[must_use]
    pub(crate) fn stdout_len(&self) -> usize {
        self.stdout.len()
    }

    /// The retained stdout as UTF-8, or the position of the first invalid
    /// byte.
    pub(crate) fn stdout_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.stdout)
    }

    /// The retained stdout with invalid UTF-8 replaced.
    #[must_use]
    pub(crate) fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The last stderr bytes under [`StderrPolicy::LogTail`], else empty.
    #[must_use]
    #[expect(dead_code, reason = "first used by the package command runner")]
    pub(crate) fn stderr_tail(&self) -> &[u8] {
        &self.stderr_tail
    }
}

/// How a run ended.
#[derive(Debug)]
pub(crate) enum RunOutcome {
    /// The child exited, successfully or not, within the limits.
    Exited(RunOutput),
    /// The deadline passed and the process group was killed.
    TimedOut,
    /// Stdout exceeded the cap and the process group was killed.
    Overflow,
    /// The cancel flag was set and the process group was killed.
    Cancelled,
    /// The run could not be set up, polled or read.
    Unavailable,
}

/// Runs `spec` to completion.
///
/// `now` is the clock the deadline is measured on. `cancel`, when given, is
/// checked on every poll; setting it kills the group within one poll interval.
#[must_use]
pub(crate) fn run(
    spec: &RunSpec<'_>,
    now: &impl Fn() -> Instant,
    cancel: Option<&AtomicBool>,
) -> RunOutcome {
    let Some(sandbox) = RunSandbox::create() else {
        return RunOutcome::Unavailable;
    };
    let mut command = build_command(spec, &sandbox);
    let Ok(mut child) = command.spawn() else {
        return RunOutcome::Unavailable;
    };
    let Some(mut stdout) = child
        .stdout
        .take()
        .and_then(|pipe| DrainedPipe::new(pipe, spec.limits.stdout_cap, Retain::First))
    else {
        terminate(&mut child);
        return RunOutcome::Unavailable;
    };
    let mut stderr = match (spec.stderr, child.stderr.take()) {
        (StderrPolicy::Discard, _) => None,
        (StderrPolicy::LogTail, pipe) => {
            let Some(drained) =
                pipe.and_then(|pipe| DrainedPipe::new(pipe, STDERR_TAIL_BYTES, Retain::Last))
            else {
                terminate(&mut child);
                return RunOutcome::Unavailable;
            };
            Some(drained)
        }
    };

    let deadline = now() + spec.limits.timeout;
    let ended = supervise(
        &mut child,
        &mut stdout,
        stderr.as_mut(),
        deadline,
        now,
        cancel,
    );
    let stderr_tail = stderr.map(DrainedPipe::finish_tail).unwrap_or_default();
    if !stderr_tail.is_empty() {
        debug!(
            stderr_tail = %String::from_utf8_lossy(&stderr_tail).escape_debug(),
            "bounded run stderr tail"
        );
    }
    match ended {
        Ended::Exited(status) => match stdout.finish() {
            Some(stdout) => RunOutcome::Exited(RunOutput {
                status,
                stdout,
                stderr_tail,
            }),
            None => RunOutcome::Overflow,
        },
        Ended::TimedOut => RunOutcome::TimedOut,
        Ended::Overflow => RunOutcome::Overflow,
        Ended::Cancelled => RunOutcome::Cancelled,
        Ended::Unavailable => RunOutcome::Unavailable,
    }
}

/// How the supervising loop ended, before the retained output is collected.
enum Ended {
    Exited(ExitStatus),
    TimedOut,
    Overflow,
    Cancelled,
    Unavailable,
}

/// Builds the command: cleared environment, fixed pairs, the sandbox
/// variables and the optional search path, a closed stdin, and a process group
/// of its own.
fn build_command(spec: &RunSpec<'_>, sandbox: &RunSandbox) -> Command {
    let mut command = Command::new(spec.program);
    command.envs(spec.seeded_env.iter().copied());
    command.env_clear().envs(spec.env.iter().cloned());
    command
        .current_dir(&sandbox.cwd_path)
        .env("LANG", RUN_LOCALE)
        .env("LC_ALL", RUN_LOCALE)
        .env("HOME", &sandbox.home_path)
        .env("XDG_CONFIG_HOME", &sandbox.xdg_config_path)
        .env("XDG_CACHE_HOME", &sandbox.xdg_cache_path)
        .env("XDG_DATA_HOME", &sandbox.xdg_data_path)
        .env("XDG_STATE_HOME", &sandbox.xdg_state_path)
        .env("XDG_RUNTIME_DIR", &sandbox.xdg_runtime_path)
        .env("TMPDIR", &sandbox.tmp_path);
    command.args(spec.args);
    if let Some(search_path) = spec.search_path {
        command.env(SEARCH_PATH_VAR, search_path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(match spec.stderr {
            StderrPolicy::Discard => Stdio::null(),
            StderrPolicy::LogTail => Stdio::piped(),
        });
    configure_process_group(&mut command);
    command
}

/// Polls the child until it exits, the deadline passes, stdout overflows or
/// `cancel` is set, and leaves the child reaped and its group killed.
fn supervise(
    child: &mut Child,
    stdout: &mut DrainedPipe<std::process::ChildStdout>,
    mut stderr: Option<&mut DrainedPipe<std::process::ChildStderr>>,
    deadline: Instant,
    now: &impl Fn() -> Instant,
    cancel: Option<&AtomicBool>,
) -> Ended {
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            terminate(child);
            return Ended::Cancelled;
        }
        if stdout.drain_available() == Drain::Overflow {
            terminate(child);
            return Ended::Overflow;
        }
        if let Some(pipe) = stderr.as_deref_mut() {
            pipe.drain_available();
        }
        match has_exited(child) {
            Ok(true) => {
                // The exited child is not reaped yet, so its pid still holds
                // the process group id and the signal reaches only the run's
                // own group.
                terminate_group(child.id());
                return match child.wait() {
                    Ok(status) => Ended::Exited(status),
                    Err(_) => Ended::Unavailable,
                };
            }
            Ok(false) if now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(false) => {
                terminate(child);
                return Ended::TimedOut;
            }
            Err(error) if child_gone(&error) => {
                // The child is no longer waitable, so its pid and group id may
                // already belong to an unrelated process and must not be
                // signalled.
                debug!(%error, "bounded run child is no longer waitable");
                return Ended::Unavailable;
            }
            Err(error) => {
                debug!(%error, "bounded run exit check failed");
                terminate(child);
                return Ended::Unavailable;
            }
        }
    }
}

/// Whether a pipe may hold more output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// The pipe is empty for now or at end of file, and the cap is not reached.
    Within,
    /// More than the cap was written to a pipe that retains its first bytes.
    Overflow,
}

/// Which bytes a [`DrainedPipe`] keeps once it holds more than its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retain {
    /// Keep the first `limit` bytes; more output is an overflow.
    First,
    /// Keep the last `limit` bytes; older output is dropped.
    Last,
}

/// The read end of a child's pipe, bounded to a byte limit.
///
/// The pipe is read without blocking while the child runs, so a child that
/// floods its output is noticed within one poll interval and a descendant that
/// keeps the pipe open past the child's exit cannot stall the daemon.
#[derive(Debug)]
struct DrainedPipe<R> {
    pipe: R,
    bytes: Vec<u8>,
    limit: usize,
    retain: Retain,
    nonblocking: bool,
}

impl<R: Read + AsFd> DrainedPipe<R> {
    /// Wraps `pipe`, or `None` when it cannot be read without blocking.
    fn new(pipe: R, limit: usize, retain: Retain) -> Option<Self> {
        let nonblocking = set_nonblocking(&pipe).ok()?;
        Some(Self {
            pipe,
            bytes: Vec::with_capacity(limit.min(READ_CHUNK_BYTES)),
            limit,
            retain,
            nonblocking,
        })
    }

    /// Moves what the pipe holds right now into the retained output.
    ///
    /// A platform without non-blocking pipe reads leaves the read to
    /// [`Self::finish`], after the child has exited.
    fn drain_available(&mut self) -> Drain {
        if !self.nonblocking {
            return Drain::Within;
        }
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        let mut taken = 0_usize;
        loop {
            match self.pipe.read(&mut chunk) {
                // End of file, or nothing more to read yet.
                Ok(0) => return Drain::Within,
                Ok(read) => {
                    taken += read;
                    self.bytes.extend_from_slice(&chunk[..read]);
                    if self.bytes.len() > self.limit {
                        match self.retain {
                            Retain::First => return Drain::Overflow,
                            Retain::Last => {
                                let excess = self.bytes.len() - self.limit;
                                self.bytes.drain(..excess);
                            }
                        }
                    }
                    if self.retain == Retain::Last && taken >= TAIL_DRAIN_BUDGET_BYTES {
                        return Drain::Within;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Drain::Within;
                }
                Err(error) => {
                    debug!(%error, "bounded run output read failed");
                    return Drain::Within;
                }
            }
        }
    }

    /// The retained first bytes once the child has exited, or `None` when it
    /// exceeded the cap or could not be read.
    fn finish(mut self) -> Option<Vec<u8>> {
        if self.nonblocking {
            return (self.drain_available() == Drain::Within).then_some(self.bytes);
        }
        // A blocking pipe is read only now: the child's process group is gone,
        // so end of file arrives once the pipe's writers have closed.
        let limit = u64::try_from(self.limit + 1).ok()?;
        (&mut self.pipe)
            .take(limit)
            .read_to_end(&mut self.bytes)
            .ok()?;
        (self.bytes.len() <= self.limit).then_some(self.bytes)
    }

    /// The retained last bytes after one final read; empty where the platform
    /// has no non-blocking read.
    fn finish_tail(mut self) -> Vec<u8> {
        self.drain_available();
        self.bytes
    }
}

/// Switches `pipe` to non-blocking reads; `Ok(false)` where the platform
/// has none.
#[cfg(unix)]
fn set_nonblocking(pipe: &impl AsFd) -> std::io::Result<bool> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};

    let flags = fcntl_getfl(pipe)?;
    fcntl_setfl(pipe, flags | OFlags::NONBLOCK)?;
    Ok(true)
}

#[cfg(not(unix))]
fn set_nonblocking(_pipe: &impl AsFd) -> std::io::Result<bool> {
    Ok(false)
}

/// Owner-private state paths for one run.
#[derive(Debug)]
pub(crate) struct RunSandbox {
    dir: PathBuf,
    home_path: PathBuf,
    xdg_config_path: PathBuf,
    xdg_cache_path: PathBuf,
    xdg_data_path: PathBuf,
    xdg_state_path: PathBuf,
    xdg_runtime_path: PathBuf,
    tmp_path: PathBuf,
    cwd_path: PathBuf,
}

impl RunSandbox {
    pub(crate) fn create() -> Option<Self> {
        for _ in 0..SANDBOX_CREATE_ATTEMPTS {
            let dir =
                std::env::temp_dir().join(format!("{SANDBOX_DIR_PREFIX}{}", ulid::Ulid::new()));
            match create_owner_only_dir(&dir) {
                Ok(()) => {
                    let sandbox = Self {
                        home_path: dir.join("home"),
                        xdg_config_path: dir.join("xdg-config"),
                        xdg_cache_path: dir.join("xdg-cache"),
                        xdg_data_path: dir.join("xdg-data"),
                        xdg_state_path: dir.join("xdg-state"),
                        xdg_runtime_path: dir.join("xdg-runtime"),
                        tmp_path: dir.join("tmp"),
                        cwd_path: dir.join("cwd"),
                        dir,
                    };
                    if sandbox
                        .private_dirs()
                        .iter()
                        .try_for_each(|path| create_owner_only_dir(path))
                        .is_err()
                    {
                        drop(sandbox);
                        return None;
                    }
                    return Some(sandbox);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// The sandbox root, which holds every directory below.
    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn private_dirs(&self) -> [&Path; 8] {
        [
            &self.home_path,
            &self.xdg_config_path,
            &self.xdg_cache_path,
            &self.xdg_data_path,
            &self.xdg_state_path,
            &self.xdg_runtime_path,
            &self.tmp_path,
            &self.cwd_path,
        ]
    }
}

impl Drop for RunSandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn create_owner_only_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_owner_only_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

/// Whether the child has exited, observed without reaping it, so its pid
/// and process group id stay reserved until `child` is waited.
///
/// An interrupted check reports the child as still running, so the caller's
/// deadline decides when to check again.
#[cfg(unix)]
fn has_exited(child: &mut Child) -> std::io::Result<bool> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};

    let pid = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid child PID")
        })?;
    match waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    ) {
        Ok(status) => Ok(status.is_some()),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Whether the child has exited.
#[cfg(not(unix))]
fn has_exited(child: &mut Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

/// Whether a failed exit check means the child is no longer waitable.
#[cfg(unix)]
fn child_gone(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ECHILD)
}

/// Whether a failed exit check means the child is no longer waitable.
#[cfg(not(unix))]
fn child_gone(_error: &std::io::Error) -> bool {
    false
}

/// Kills the child's process group and the child, then reaps it.
fn terminate(child: &mut Child) {
    terminate_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_group(child_id: u32) {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(child_id) else {
            return;
        };
        #[expect(
            unsafe_code,
            reason = "libc::kill is required to terminate a run's process subtree"
        )]
        // SAFETY: the child starts in a process group whose id is its positive
        // pid; negating it targets only that group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child_id;
}
