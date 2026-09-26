//! Runs `/bin/launchctl` with argv only, a deadline, and bounded output.
//!
//! Results are keyed on the exit status alone. `launchctl` prints localized,
//! unversioned text, so its output is kept only as bounded diagnostics and is
//! never matched. The status table was recorded on GitHub-hosted macOS 14.8
//! and 15.7 runners:
//!
//! | Status | Meaning (errno)            | Observed for                                  |
//! |--------|----------------------------|-----------------------------------------------|
//! | 0      | success                    | `print`, `bootstrap`, `bootout`               |
//! | 3      | `ESRCH`                    | `bootout` of an absent label                  |
//! | 5      | `EIO`                      | `bootstrap` of an already loaded label        |
//! | 37     | `EALREADY`                 | an operation already in progress on the label |
//! | 112    | domain not found           | `print`/`bootstrap` in `gui/<unknown uid>`    |
//! | 113    | service not found          | `print` of an absent label                    |
//!
//! Every other status, and termination by a signal, is [`Status::Unmapped`].

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

// Rust guideline compliant 2026-09-24

/// The only `launchctl` executable the backend runs.
///
/// An absolute system path, so neither `PATH` nor the working directory can
/// substitute another program.
pub(crate) const LAUNCHCTL: &str = "/bin/launchctl";

/// Most bytes kept from each of standard output and standard error.
///
/// Enough for any diagnostic `launchctl` prints for one label, while a chatty
/// or hostile child cannot grow daemon memory; bytes beyond the cap are read
/// and discarded so the child never blocks on a full pipe.
pub(crate) const OUTPUT_CAP: usize = 16 * 1024;

/// Read buffer used while draining one output pipe.
const READ_CHUNK: usize = 4 * 1024;

/// `launchctl` exit statuses the backend distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// Exit status 0.
    Success,
    /// Exit status 3 (`ESRCH`): `bootout` found no such job.
    NoSuchProcess,
    /// Exit status 5 (`EIO`): `bootstrap` could not load the definition, which
    /// includes a label that is already loaded.
    InputOutput,
    /// Exit status 37 (`EALREADY`): another operation on the label is in
    /// progress.
    InProgress,
    /// Exit status 112: the domain, such as `gui/<uid>`, does not exist.
    NoSuchDomain,
    /// Exit status 113: the service is not loaded in the domain.
    NoSuchService,
    /// Any other exit status or a signal.
    Unmapped,
}

impl Status {
    /// Maps a raw exit code; `None` means the child was killed by a signal.
    pub(crate) fn from_code(code: Option<i32>) -> Self {
        match code {
            Some(0) => Self::Success,
            Some(3) => Self::NoSuchProcess,
            Some(5) => Self::InputOutput,
            Some(37) => Self::InProgress,
            Some(112) => Self::NoSuchDomain,
            Some(113) => Self::NoSuchService,
            _ => Self::Unmapped,
        }
    }
}

/// Completed `launchctl` invocation.
#[derive(Debug)]
pub(crate) struct Completion {
    /// Mapped exit status.
    pub(crate) status: Status,
    subcommand: &'static str,
    exit: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Completion {
    /// Returns a diagnostic error describing this invocation.
    pub(crate) fn failure(&self) -> Failure {
        Failure {
            subcommand: self.subcommand,
            code: self.exit.code(),
            stdout: String::from_utf8_lossy(&self.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&self.stderr).into_owned(),
        }
    }

    /// Returns the captured standard output, bounded by [`OUTPUT_CAP`].
    #[cfg(test)]
    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns the captured standard error, bounded by [`OUTPUT_CAP`].
    #[cfg(test)]
    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// Unexpected `launchctl` outcome kept as a diagnostic error source.
///
/// The output text is carried for humans reading logs only; no code path
/// inspects it.
#[derive(Debug)]
pub(crate) struct Failure {
    subcommand: &'static str,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "launchctl {} exited with status {code}", self.subcommand)?,
            None => write!(
                f,
                "launchctl {} was terminated by a signal",
                self.subcommand
            )?,
        }
        for output in [&self.stderr, &self.stdout] {
            let output = output.trim();
            if !output.is_empty() {
                write!(f, ": {output}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

/// Failure to obtain any exit status from `launchctl`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    /// The executable could not be started.
    #[error("could not start launchctl {subcommand}: {source}")]
    Spawn {
        /// Subcommand that was requested.
        subcommand: &'static str,
        /// Operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The child did not exit before the deadline; it was killed and reaped.
    #[error("launchctl {subcommand} exceeded its deadline")]
    Timeout {
        /// Subcommand that was requested.
        subcommand: &'static str,
    },
    /// Waiting for the child or reading its output failed.
    #[error("could not collect launchctl {subcommand}: {source}")]
    Io {
        /// Subcommand that was requested.
        subcommand: &'static str,
        /// Operating-system error.
        #[source]
        source: std::io::Error,
    },
}

/// Bounded `launchctl` runner.
#[derive(Debug, Clone)]
pub(crate) struct Launchctl {
    program: PathBuf,
    leading: Vec<String>,
    deadline: Duration,
}

impl Launchctl {
    /// Creates a runner for [`LAUNCHCTL`] whose commands end after `deadline`.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(dead_code, reason = "only the macOS backend runs the real launchctl")
    )]
    pub(crate) fn new(deadline: Duration) -> Self {
        Self {
            program: PathBuf::from(LAUNCHCTL),
            leading: Vec::new(),
            deadline,
        }
    }

    /// Creates a runner for a fake `program` that receives `leading`
    /// arguments before the `launchctl` ones.
    #[cfg(test)]
    pub(crate) fn with_program(program: &Path, leading: &[&str], deadline: Duration) -> Self {
        Self {
            program: program.to_path_buf(),
            leading: leading.iter().map(|value| (*value).to_owned()).collect(),
            deadline,
        }
    }

    /// Returns the configured per-command deadline.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(dead_code, reason = "only the macOS backend extends deadlines")
    )]
    pub(crate) fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Runs `launchctl <subcommand> <arguments…>` within the configured deadline.
    pub(crate) async fn run(
        &self,
        subcommand: &'static str,
        arguments: &[&OsStr],
    ) -> Result<Completion, RunError> {
        self.run_within(subcommand, arguments, self.deadline).await
    }

    /// Runs one command within an explicit `deadline`.
    ///
    /// On expiry the child is killed and reaped before [`RunError::Timeout`]
    /// is returned, so no `launchctl` process outlives the call.
    pub(crate) async fn run_within(
        &self,
        subcommand: &'static str,
        arguments: &[&OsStr],
        deadline: Duration,
    ) -> Result<Completion, RunError> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.leading)
            .arg(subcommand)
            .args(arguments)
            .env_clear()
            .current_dir(Path::new("/"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|source| RunError::Spawn { subcommand, source })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collected = tokio::time::timeout(deadline, async {
            tokio::join!(drain(stdout), drain(stderr), child.wait())
        })
        .await;
        let Ok((stdout, stderr, exit)) = collected else {
            // `kill` also waits, so the child is reaped before returning. A
            // child that exited just before the kill is reaped the same way.
            if let Err(source) = child.kill().await {
                return Err(RunError::Io { subcommand, source });
            }
            return Err(RunError::Timeout { subcommand });
        };
        let io = |source| RunError::Io { subcommand, source };
        let exit = exit.map_err(io)?;
        Ok(Completion {
            status: Status::from_code(exit.code()),
            subcommand,
            exit,
            stdout: stdout.map_err(io)?,
            stderr: stderr.map_err(io)?,
        })
    }
}

/// Reads a pipe to its end, keeping at most [`OUTPUT_CAP`] bytes.
async fn drain(pipe: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let Some(mut pipe) = pipe else {
        return Ok(kept);
    };
    let mut chunk = [0_u8; READ_CHUNK];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok(kept);
        }
        let room = OUTPUT_CAP - kept.len();
        kept.extend_from_slice(&chunk[..read.min(room)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shell that plays `launchctl`: `sh -c <script> launchctl <argv…>`.
    const SHELL: &str = "/bin/sh";

    fn fake(script: &str, deadline: Duration) -> Launchctl {
        Launchctl::with_program(Path::new(SHELL), &["-c", script, "launchctl"], deadline)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime starts")
    }

    #[test]
    fn exit_statuses_follow_the_recorded_table() {
        let table = [
            (0, Status::Success),
            (3, Status::NoSuchProcess),
            (5, Status::InputOutput),
            (37, Status::InProgress),
            (112, Status::NoSuchDomain),
            (113, Status::NoSuchService),
            (1, Status::Unmapped),
            (36, Status::Unmapped),
            (114, Status::Unmapped),
        ];
        runtime().block_on(async {
            for (code, expected) in table {
                let completion = fake(&format!("exit {code}"), Duration::from_secs(10))
                    .run("print", &[])
                    .await
                    .expect("fake launchctl exits");
                assert_eq!(completion.status, expected, "exit status {code}");
                assert_eq!(completion.failure().code, Some(code));
            }
        });
        assert_eq!(Status::from_code(None), Status::Unmapped);
    }

    #[test]
    fn arguments_are_passed_as_argv_without_a_shell() {
        let hostile = "gui/501/a b;$(touch /nonexistent)'\"";
        let completion = runtime()
            .block_on(
                fake(r#"printf '%s\n' "$@""#, Duration::from_secs(10))
                    .run("bootout", &[OsStr::new(hostile)]),
            )
            .expect("fake launchctl exits");
        assert_eq!(completion.status, Status::Success);
        assert_eq!(
            completion.stdout(),
            format!("bootout\n{hostile}\n").as_bytes()
        );
    }

    #[test]
    fn a_child_past_its_deadline_is_killed_and_reaped() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let pid_file = directory.path().join("pid");
        let script = format!("echo $$ > '{}'; exec /bin/sleep 30", pid_file.display());
        let started = std::time::Instant::now();
        let result =
            runtime().block_on(fake(&script, Duration::from_millis(500)).run("print", &[]));
        assert!(matches!(
            result,
            Err(RunError::Timeout {
                subcommand: "print"
            })
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("fake launchctl recorded its pid")
            .trim()
            .parse()
            .expect("pid is decimal");
        // A zombie still answers signal 0, so ESRCH proves the child was reaped.
        let probe = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
        assert_eq!(probe, Err(nix::errno::Errno::ESRCH));
    }

    #[test]
    fn flooding_output_is_capped_without_blocking_the_child() {
        // Writes 64x the cap to both pipes, then exits with a mapped status.
        let script =
            "/usr/bin/head -c 1048576 /dev/zero; /usr/bin/head -c 1048576 /dev/zero >&2; exit 113";
        let completion = runtime()
            .block_on(fake(script, Duration::from_secs(20)).run("print", &[]))
            .expect("flooding child completes");
        assert_eq!(completion.status, Status::NoSuchService);
        assert_eq!(completion.stdout().len(), OUTPUT_CAP);
        assert_eq!(completion.stderr().len(), OUTPUT_CAP);
    }

    #[test]
    fn diagnostics_keep_bounded_stderr_only_as_text() {
        let completion = runtime()
            .block_on(
                fake(
                    "echo 'Bootstrap failed: 5' >&2; exit 42",
                    Duration::from_secs(10),
                )
                .run("bootstrap", &[]),
            )
            .expect("fake launchctl exits");
        assert_eq!(completion.status, Status::Unmapped);
        assert_eq!(
            completion.failure().to_string(),
            "launchctl bootstrap exited with status 42: Bootstrap failed: 5"
        );
    }

    #[test]
    fn a_missing_executable_is_a_spawn_error() {
        let result = runtime().block_on(
            Launchctl::with_program(
                Path::new("/nonexistent/launchctl"),
                &[],
                Duration::from_secs(1),
            )
            .run("print", &[]),
        );
        assert!(matches!(result, Err(RunError::Spawn { .. })));
    }
}
