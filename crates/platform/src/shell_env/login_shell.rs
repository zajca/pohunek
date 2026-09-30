//! Bounded discovery of the `PATH` a user's login shell builds.

use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::search_path::{SearchPath, SearchPathError};

/// Executable printing the environment `PATH` of the login shell.
///
/// An absolute path keeps the probe independent of the `PATH` the shell built;
/// `/usr/bin/printenv` exists on macOS and every mainstream Linux.
pub const PRINTENV_EXECUTABLE: &str = "/usr/bin/printenv";

/// `PATH` the probe shell starts with.
///
/// The value launchd gives its own jobs. Login startup files (`path_helper`
/// on macOS) extend it; starting from it keeps the result independent of
/// whichever `PATH` the installing process happened to carry.
pub const PROBE_BASELINE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// `TERM` value handed to the probe so startup files skip terminal setup.
const PROBE_TERM: &str = "dumb";

/// Hard deadline of the login-shell `PATH` discovery.
///
/// A login shell that sources `nvm`, `pyenv`, or `conda` initialisation can
/// need several seconds; ten seconds tolerates that while a wedged startup file
/// only delays the caller by that much before the fallback directory list
/// applies. The probe's process group is killed at the deadline.
pub const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes of login-shell stdout kept by the `PATH` discovery.
///
/// The machine-readable part is one `PATH` line of at most 4 KiB; the rest
/// is room for a startup banner. A shell printing more is killed and treated
/// as failed.
pub const LOGIN_SHELL_OUTPUT: usize = 64 * 1024;

/// Login shell used for discovery when `$SHELL` is unset or unusable.
///
/// `zsh` is the default login shell of every supported macOS release and
/// `/bin/zsh` ships with the system.
pub const DEFAULT_LOGIN_SHELL: &str = "/bin/zsh";

/// Prefix of the random sentinel lines delimiting the machine-readable output.
const SENTINEL_PREFIX: &str = "__POHUNEK_PATH_";

/// Random bytes in one sentinel; user startup output cannot predict them.
const SENTINEL_RANDOM_BYTES: usize = 12;

/// Inputs of one login-shell probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginShellSpec {
    /// Absolute path of the user's login shell (`$SHELL`).
    pub shell: PathBuf,
    /// Absolute path of the `printenv` executable, [`PRINTENV_EXECUTABLE`].
    pub printenv: PathBuf,
    /// Extra non-secret variables the shell needs to find its startup files
    /// (`HOME`, `USER`, `LOGNAME`); the probe starts from an empty environment.
    pub environment: Vec<(String, String)>,
    /// Hard deadline for the whole probe; the process group is killed after it.
    pub timeout: Duration,
    /// Most stdout bytes read before the probe is killed.
    pub max_output_bytes: usize,
}

/// A successfully discovered login-shell `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginShellDiscovery {
    /// The sanitized directories.
    pub path: SearchPath,
    /// Entries of the printed value dropped by sanitization.
    pub dropped: usize,
}

/// Reports why login-shell discovery yielded no usable `PATH`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoginShellError {
    /// The shell or `printenv` path is not an absolute executable file.
    #[error("login shell path is unusable: {reason}")]
    InvalidExecutable {
        /// Why the path was rejected; never contains the path.
        reason: &'static str,
    },
    /// The probe could not be started or observed.
    #[error("login shell probe failed to run")]
    Io(#[source] io::Error),
    /// The shell did not finish within the deadline and was killed.
    #[error("login shell probe exceeded {timeout:?} and was killed")]
    Timeout {
        /// The deadline that elapsed.
        timeout: Duration,
    },
    /// The shell printed more than the output bound and was killed.
    #[error("login shell probe printed more than {limit} bytes and was killed")]
    OutputTooLarge {
        /// The output bound.
        limit: usize,
    },
    /// The shell exited unsuccessfully.
    #[error("login shell probe exited unsuccessfully ({status})")]
    Failed {
        /// Exit code or terminating signal in words.
        status: String,
    },
    /// The sentinel-delimited value is missing, repeated, or not one line.
    #[error("login shell probe output holds no single delimited value")]
    MalformedOutput,
    /// The delimited value is not UTF-8.
    #[error("login shell probe printed a non-UTF-8 value")]
    NotUtf8,
    /// The delimited value is not a usable `PATH`.
    #[error("login shell probe printed an unusable PATH")]
    UnusablePath(#[source] SearchPathError),
}

/// Runs the user's login shell once and returns the `PATH` it builds.
///
/// The shell runs as `<shell> -l -c <script>`, never interactively, with a
/// null stdin and stderr and only the [`LoginShellSpec::environment`] plus a
/// baseline `PATH`. Its own startup output is ignored: the value counts only
/// between two random sentinel lines, and the script runs `printenv PATH`
/// through an absolute path. The probe is a separate process group killed on
/// timeout or oversized output.
///
/// # Errors
///
/// Returns [`LoginShellError`] for an unusable shell, a spawn failure, a
/// timeout, oversized output, an unsuccessful exit, malformed or non-UTF-8
/// output, and a value without any usable directory. A failure never yields
/// a partial or guessed value.
pub fn discover_login_shell_path(
    spec: &LoginShellSpec,
) -> Result<LoginShellDiscovery, LoginShellError> {
    require_executable(&spec.shell)?;
    require_executable(&spec.printenv)?;
    let sentinel = random_sentinel().map_err(LoginShellError::Io)?;
    let printenv = spec
        .printenv
        .to_str()
        .ok_or(LoginShellError::InvalidExecutable {
            reason: "printenv path is not UTF-8",
        })?;
    if printenv.contains('\'') {
        return Err(LoginShellError::InvalidExecutable {
            reason: "printenv path contains a quote",
        });
    }
    let script =
        format!("printf '%s\\n' '{sentinel}'; '{printenv}' PATH; printf '%s\\n' '{sentinel}'");

    let mut command = Command::new(&spec.shell);
    command
        .arg("-l")
        .arg("-c")
        .arg(script)
        .env_clear()
        .env("PATH", PROBE_BASELINE_PATH)
        .env("TERM", PROBE_TERM)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    for (name, value) in &spec.environment {
        command.env(name, value);
    }
    let output = run_bounded(command, spec.timeout, spec.max_output_bytes)?;
    if !output.status.success() {
        return Err(LoginShellError::Failed {
            status: describe(output.status),
        });
    }
    let value = delimited_value(&output.stdout, &sentinel)?;
    let sanitized = SearchPath::sanitize(value, true).map_err(LoginShellError::UnusablePath)?;
    Ok(LoginShellDiscovery {
        path: sanitized.path,
        dropped: sanitized.dropped,
    })
}

fn require_executable(path: &Path) -> Result<(), LoginShellError> {
    if !path.is_absolute() {
        return Err(LoginShellError::InvalidExecutable {
            reason: "not absolute",
        });
    }
    if !super::executable::is_executable_file(path) {
        return Err(LoginShellError::InvalidExecutable {
            reason: "not an executable file",
        });
    }
    Ok(())
}

fn random_sentinel() -> io::Result<String> {
    let mut random = [0_u8; SENTINEL_RANDOM_BYTES];
    getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let mut sentinel = String::from(SENTINEL_PREFIX);
    for byte in random {
        // Writing to a String cannot fail.
        let _ = write!(sentinel, "{byte:02x}");
    }
    sentinel.push_str("__");
    Ok(sentinel)
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown status".to_owned(),
    }
}

/// Extracts the single line between the two sentinel lines.
fn delimited_value<'a>(stdout: &'a [u8], sentinel: &str) -> Result<&'a str, LoginShellError> {
    let lines: Vec<&[u8]> = stdout.split(|byte| *byte == b'\n').collect();
    let marks: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == sentinel.as_bytes())
        .map(|(index, _)| index)
        .collect();
    let [begin, end] = marks[..] else {
        return Err(LoginShellError::MalformedOutput);
    };
    if end != begin + 2 {
        return Err(LoginShellError::MalformedOutput);
    }
    std::str::from_utf8(lines[begin + 1]).map_err(|_invalid| LoginShellError::NotUtf8)
}

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
}

enum Event {
    Output(io::Result<(Vec<u8>, bool)>),
    Exit(io::Result<ExitStatus>),
}

/// Runs `command` with piped stdout under a deadline and an output bound.
///
/// The child must lead its own process group, which is killed on every return
/// path. Reader and waiter threads are detached on failure: killing the group
/// closes the pipe and reaps the child.
fn run_bounded(
    mut command: Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<Captured, LoginShellError> {
    let mut child = command.spawn().map_err(LoginShellError::Io)?;
    let pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| LoginShellError::Io(io::Error::other("probe stdout was not captured")))?;
    let (sender, receiver) = mpsc::channel();
    let reader_sender = sender.clone();
    let bound = u64::try_from(max_output_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    std::thread::Builder::new()
        .name("shell-env-reader".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = (&mut stdout).take(bound).read_to_end(&mut bytes).map(|_| {
                let truncated = bytes.len() > max_output_bytes;
                (bytes, truncated)
            });
            // The receiver is gone once the probe failed; nothing to report.
            drop(reader_sender.send(Event::Output(result)));
        })
        .map_err(|source| {
            kill_group(pid);
            LoginShellError::Io(source)
        })?;
    std::thread::Builder::new()
        .name("shell-env-waiter".to_owned())
        .spawn(move || {
            drop(sender.send(Event::Exit(child.wait())));
        })
        .map_err(|source| {
            kill_group(pid);
            LoginShellError::Io(source)
        })?;

    let deadline = Instant::now() + timeout;
    let mut stdout_bytes: Option<Vec<u8>> = None;
    let mut status: Option<ExitStatus> = None;
    while stdout_bytes.is_none() || status.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(Event::Output(Ok((bytes, truncated)))) => {
                if truncated {
                    kill_group(pid);
                    return Err(LoginShellError::OutputTooLarge {
                        limit: max_output_bytes,
                    });
                }
                stdout_bytes = Some(bytes);
            }
            Ok(Event::Exit(Ok(observed))) => status = Some(observed),
            Ok(Event::Output(Err(source)) | Event::Exit(Err(source))) => {
                kill_group(pid);
                return Err(LoginShellError::Io(source));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill_group(pid);
                return Err(LoginShellError::Timeout { timeout });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                kill_group(pid);
                return Err(LoginShellError::Io(io::Error::other(
                    "probe observers ended early",
                )));
            }
        }
    }
    // Startup files may leave background children behind, even with their
    // stdout redirected; the probe owns its group, so nothing of it survives.
    // The group stays valid for the kill: a leftover member keeps its id
    // taken, and without one the signal finds no process.
    kill_group(pid);
    match (status, stdout_bytes) {
        (Some(status), Some(stdout)) => Ok(Captured { status, stdout }),
        _ => Err(LoginShellError::MalformedOutput),
    }
}

/// Kills the probe's process group; an already gone group is fine.
fn kill_group(pid: u32) {
    let Ok(raw) = i32::try_from(pid) else { return };
    if let Some(group) = rustix::process::Pid::from_raw(raw) {
        // ESRCH means the group already exited.
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// Writes an executable script into `dir` and returns its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
        path
    }

    fn spec(shell: PathBuf) -> LoginShellSpec {
        LoginShellSpec {
            shell,
            printenv: PathBuf::from(PRINTENV_EXECUTABLE),
            environment: Vec::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 64 * 1024,
        }
    }

    /// A shell that exports `path` and then runs the probe script like a
    /// real login shell would after sourcing its startup files.
    fn fake_shell(dir: &Path, prelude: &str, path: &str) -> PathBuf {
        script(
            dir,
            "fake-shell",
            &format!("{prelude}\nPATH='{path}'; export PATH\n[ \"$1\" = -l ] && [ \"$2\" = -c ] || exit 64\nexec /bin/sh -c \"$3\""),
        )
    }

    #[test]
    fn discovers_the_path_the_login_shell_builds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let brew = dir.path().join("opt/homebrew/bin");
        let local = dir.path().join("home/.local/bin");
        fs::create_dir_all(&brew).expect("brew");
        fs::create_dir_all(&local).expect("local");
        let path = format!("{}:{}", brew.display(), local.display());
        let shell = fake_shell(dir.path(), "", &path);
        let found = discover_login_shell_path(&spec(shell)).expect("discovery");
        assert_eq!(found.path.entries(), [brew, local]);
        assert_eq!(found.dropped, 0);
    }

    #[test]
    fn ignores_startup_noise_before_and_after_the_sentinels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let noise = "echo 'welcome back'; echo '/fake/PATH=decoy'; printf '\\377\\376junk\\n'";
        let shell = fake_shell(dir.path(), noise, &bin.display().to_string());
        let found = discover_login_shell_path(&spec(shell)).expect("discovery");
        assert_eq!(found.path.entries(), [bin]);
    }

    #[test]
    fn a_decoy_line_cannot_spoof_the_sentinel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let shell = fake_shell(
            dir.path(),
            "echo __POHUNEK_PATH_000000000000000000000000__; echo /decoy; echo __POHUNEK_PATH_000000000000000000000000__",
            &bin.display().to_string(),
        );
        let found = discover_login_shell_path(&spec(shell)).expect("discovery");
        assert_eq!(found.path.entries(), [bin]);
    }

    #[test]
    fn keeps_absolute_existing_directories_with_spaces_quotes_and_unicode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spaced = dir.path().join("my tools/bin dir");
        let unicode = dir.path().join("nástroje/日本語");
        let quoted = dir.path().join("it's \"here\"");
        for entry in [&spaced, &unicode, &quoted] {
            fs::create_dir_all(entry).expect("dir");
        }
        // Single quotes cannot sit inside the fake shell's quoted PATH, so
        // the value is passed through a file the fake shell reads.
        let value = format!(
            "{}:{}:{}",
            spaced.display(),
            unicode.display(),
            quoted.display()
        );
        let value_file = dir.path().join("path-value");
        fs::write(&value_file, &value).expect("value");
        let shell = script(
            dir.path(),
            "fake-shell",
            &format!(
                "PATH=\"$(cat '{}')\"; export PATH\nexec /bin/sh -c \"$3\"",
                value_file.display()
            ),
        );
        let found = discover_login_shell_path(&spec(shell)).expect("discovery");
        assert_eq!(found.path.entries(), [spaced, unicode, quoted]);
    }

    #[test]
    fn drops_empty_relative_dot_duplicate_and_missing_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let value = format!(
            ":.:relative/bin:{bin}:{bin}:{missing}:{bin}/../bin",
            bin = bin.display(),
            missing = dir.path().join("missing").display()
        );
        let shell = fake_shell(dir.path(), "", &value);
        let found = discover_login_shell_path(&spec(shell)).expect("discovery");
        assert_eq!(found.path.entries(), [bin]);
        assert_eq!(found.dropped, 6);
    }

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

    #[test]
    fn a_hanging_shell_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("grandchild-pid");
        // The grandchild keeps the stdout pipe open, so only a group kill
        // ends the probe. The deadline leaves the script ample time to start.
        let shell = script(
            dir.path(),
            "hang-shell",
            &format!("sleep 300 &\necho $! > '{}'\nwait", marker.display()),
        );
        let mut hanging = spec(shell);
        hanging.timeout = Duration::from_secs(3);
        let error = discover_login_shell_path(&hanging).expect_err("must time out");
        assert!(
            matches!(error, LoginShellError::Timeout { .. }),
            "{error:?}"
        );
        let pid: i32 = fs::read_to_string(&marker)
            .expect("marker")
            .trim()
            .parse()
            .expect("pid");
        let pid = rustix::process::Pid::from_raw(pid).expect("pid");
        // The kill is asynchronous with the reaper; wait for the grandchild
        // to disappear for a bounded time.
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_is_running(pid) {
            assert!(Instant::now() < deadline, "grandchild survived the kill");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_background_child_with_redirected_stdout_does_not_outlive_a_successful_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let marker = dir.path().join("background-pid");
        // The background child detaches its stdio, so the pipe closes when the
        // shell exits and only the group kill can end it.
        let shell = script(
            dir.path(),
            "background-shell",
            &format!(
                "sleep 300 >/dev/null 2>&1 </dev/null &\necho $! > '{}'\nPATH='{}'; export PATH\nexec /bin/sh -c \"$3\"",
                marker.display(),
                bin.display()
            ),
        );
        let found = discover_login_shell_path(&spec(shell)).expect("discovery");
        assert_eq!(found.path.entries(), [bin]);
        let pid: i32 = fs::read_to_string(&marker)
            .expect("marker")
            .trim()
            .parse()
            .expect("pid");
        let pid = rustix::process::Pid::from_raw(pid).expect("pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_is_running(pid) {
            assert!(Instant::now() < deadline, "background child survived");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_shell_printing_nothing_is_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = script(dir.path(), "silent-shell", "exit 0");
        let error = discover_login_shell_path(&spec(shell)).expect_err("no output");
        assert!(
            matches!(error, LoginShellError::MalformedOutput),
            "{error:?}"
        );
    }

    #[test]
    fn junk_without_sentinels_is_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = script(dir.path(), "junk-shell", "echo '/usr/bin:/bin'\nexit 0");
        let error = discover_login_shell_path(&spec(shell)).expect_err("junk");
        assert!(
            matches!(error, LoginShellError::MalformedOutput),
            "{error:?}"
        );
    }

    #[test]
    fn a_failing_shell_is_reported_with_its_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = script(dir.path(), "failing-shell", "echo boom\nexit 3");
        let error = discover_login_shell_path(&spec(shell)).expect_err("failure");
        assert!(
            matches!(&error, LoginShellError::Failed { status } if status == "exit code 3"),
            "{error:?}"
        );
    }

    #[test]
    fn a_multiline_value_is_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let value_file = dir.path().join("value");
        fs::write(&value_file, format!("{}\n/usr/bin", bin.display())).expect("value");
        let shell = script(
            dir.path(),
            "fake-shell",
            &format!(
                "PATH=\"$(cat '{}')\"; export PATH\nexec /bin/sh -c \"$3\"",
                value_file.display()
            ),
        );
        let error = discover_login_shell_path(&spec(shell)).expect_err("multiline");
        assert!(
            matches!(error, LoginShellError::MalformedOutput),
            "{error:?}"
        );
    }

    #[test]
    fn a_path_with_a_control_character_is_unusable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let value_file = dir.path().join("value");
        fs::write(&value_file, "/usr/bin\t:/bin").expect("value");
        let shell = script(
            dir.path(),
            "fake-shell",
            &format!(
                "PATH=\"$(cat '{}')\"; export PATH\nexec /bin/sh -c \"$3\"",
                value_file.display()
            ),
        );
        let error = discover_login_shell_path(&spec(shell)).expect_err("control");
        assert!(
            matches!(
                error,
                LoginShellError::UnusablePath(SearchPathError::ControlCharacter)
            ),
            "{error:?}"
        );
    }

    #[test]
    fn oversized_output_kills_the_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = script(dir.path(), "flood-shell", "yes 'flood flood flood'");
        let mut flooding = spec(shell);
        flooding.max_output_bytes = 1024;
        let error = discover_login_shell_path(&flooding).expect_err("flood");
        assert!(
            matches!(error, LoginShellError::OutputTooLarge { limit: 1024 }),
            "{error:?}"
        );
    }

    #[test]
    fn the_probe_never_runs_interactively_and_starts_from_a_clean_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let report = dir.path().join("report");
        // The fake shell records its arguments, environment, and stdin, then
        // behaves like a successful shell.
        let shell = script(
            dir.path(),
            "recording-shell",
            &format!(
                "printf '%s|' \"$@\" > '{report}.args'\n/usr/bin/env > '{report}.env'\nif read -r line; then echo open > '{report}.stdin'; fi\nPATH='{bin}'; export PATH\nexec /bin/sh -c \"$3\"",
                report = report.display(),
                bin = bin.display()
            ),
        );
        let mut probing = spec(shell);
        probing.environment = vec![("HOME".to_owned(), "/home/probe".to_owned())];
        discover_login_shell_path(&probing).expect("discovery");
        let args = fs::read_to_string(format!("{}.args", report.display())).expect("args");
        assert!(args.starts_with("-l|-c|"), "{args}");
        assert!(!args.contains("-i|"), "{args}");
        let environment = fs::read_to_string(format!("{}.env", report.display())).expect("env");
        let names: std::collections::BTreeSet<&str> = environment
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        // `PWD`, `OLDPWD`, `SHLVL`, and `_` are added by `sh` itself.
        let expected = ["HOME", "PATH", "TERM", "PWD", "OLDPWD", "SHLVL", "_"];
        assert!(
            names.iter().all(|name| expected.contains(name)),
            "unexpected variables: {names:?}"
        );
        assert!(environment.contains("HOME=/home/probe"), "{environment}");
        assert!(environment.contains("TERM=dumb"), "{environment}");
        assert!(
            !Path::new(&format!("{}.stdin", report.display())).exists(),
            "stdin must be null"
        );
    }

    #[test]
    fn a_relative_or_missing_shell_is_rejected_before_running() {
        for shell in ["sh", "/nonexistent/shell"] {
            let error = discover_login_shell_path(&spec(PathBuf::from(shell))).expect_err("shell");
            assert!(
                matches!(error, LoginShellError::InvalidExecutable { .. }),
                "{shell}: {error:?}"
            );
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = dir.path().join("plain");
        fs::write(&plain, "not executable").expect("plain");
        let error = discover_login_shell_path(&spec(plain)).expect_err("plain file");
        assert!(matches!(error, LoginShellError::InvalidExecutable { .. }));
    }
}
