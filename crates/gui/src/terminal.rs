//! Stock-terminal attach: private self-deleting `.command` scripts, the
//! bounded `open` invocation, and detached child processes.
//!
//! The macOS Terminal launch never builds `AppleScript` or shell text from
//! session data. The GUI writes an owner-only script whose arguments are
//! single-quoted byte-for-byte, then asks `open -a Terminal <script>` (an argv
//! array) to run it. Terminal executes the script through its shebang, so no
//! Automation permission prompt is involved.

// Rust guideline compliant 2026-09-30
#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pohunek_platform::filesystem::{FsError, TrustedDir};
use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
use serde::Deserialize;
use thiserror::Error;

/// Mode of the script directory and of every script in it: owner-only, with
/// the execute bit Terminal needs to run a `.command` file.
const PRIVATE_MODE: u32 = 0o700;

/// Directory below the pohunek runtime root that holds attach scripts.
const SCRIPT_DIR_NAME: &str = "gui-attach";

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
    #[error("`{}` exited unsuccessfully ({status})", opener.display())]
    OpenerFailed { opener: PathBuf, status: ExitStatus },
    #[error("`{}` did not finish within {timeout:?} and was killed", opener.display())]
    OpenerTimeout { opener: PathBuf, timeout: Duration },
    #[error("waiting for `{}` failed: {source}", opener.display())]
    OpenerWait { opener: PathBuf, source: io::Error },
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
pub(crate) fn command_script(argv: &[OsString]) -> Result<Vec<u8>, ScriptError> {
    let Some(program) = argv.first() else {
        return Err(ScriptError::EmptyProgram);
    };
    if program.is_empty() {
        return Err(ScriptError::EmptyProgram);
    }
    if argv.iter().any(|word| word.as_bytes().contains(&0)) {
        return Err(ScriptError::NulByte);
    }
    let mut script = Vec::new();
    script.extend_from_slice(SHEBANG);
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
    ) -> Self {
        Self {
            terminal,
            opener,
            script_dir,
            open_timeout,
        }
    }

    /// Script directory below the pohunek runtime root.
    pub(crate) fn default_script_dir() -> Result<PathBuf, pohunek_paths::PathError> {
        pohunek_paths::runtime_dir().map(|root| root.join(SCRIPT_DIR_NAME))
    }

    /// Writes the script for `argv`, hands it to the opener, and waits a
    /// bounded time for the opener to exit.
    ///
    /// The script is removed again when the opener fails or is killed; on
    /// success the script deletes itself when Terminal runs it.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] for an unbuildable script, a directory or
    /// file that is unsafe or unwritable, an opener that cannot start, exits
    /// unsuccessfully, or exceeds the timeout.
    pub(crate) fn launch(&self, argv: &[OsString]) -> Result<(), TerminalError> {
        let contents = command_script(argv)?;
        let script = self.write_script(&contents)?;
        let mut command = Command::new(&self.opener);
        command
            .arg("-a")
            .arg(self.terminal.application())
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let outcome = run_bounded(&mut command, self.open_timeout);
        let result = match outcome {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(TerminalError::OpenerFailed {
                opener: self.opener.clone(),
                status: output.status,
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
        };
        if result.is_err() {
            // The script never ran, so it removes itself nowhere else.
            let _ = std::fs::remove_file(&script);
        }
        result
    }

    /// Creates the script exclusively in the owner-private directory.
    ///
    /// `TrustedDir` validates every ancestor and the directory's owner and
    /// mode without following symlinks, and `create_file` opens with
    /// `O_CREAT | O_EXCL | O_NOFOLLOW`, so a pre-planted name or link fails
    /// instead of being reused.
    fn write_script(&self, contents: &[u8]) -> Result<PathBuf, TerminalError> {
        let directory = TrustedDir::open_or_create_absolute(&self.script_dir, PRIVATE_MODE)
            .map_err(TerminalError::ScriptDirectory)?;
        let name = script_name()?;
        directory
            .create_file(&name, contents, PRIVATE_MODE)
            .map_err(TerminalError::ScriptWrite)?;
        Ok(self.script_dir.join(name))
    }
}

fn script_name() -> Result<OsString, TerminalError> {
    let mut random = [0_u8; SCRIPT_NAME_RANDOM_BYTES];
    getrandom::getrandom(&mut random).map_err(|error| TerminalError::Random(error.to_string()))?;
    let mut name = String::from("attach-");
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
    Timeout,
}

/// Exit status of a bounded run and any captured standard error.
#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: ExitStatus,
    /// At most [`STDERR_CAPTURE_LIMIT`] bytes, empty unless the command piped stderr.
    pub(crate) stderr: Vec<u8>,
}

/// Most standard-error bytes kept from a bounded run; classification needs
/// only a short diagnostic line.
const STDERR_CAPTURE_LIMIT: u64 = 4096;

/// Runs `command` and waits at most `timeout` for it to exit.
///
/// A helper thread blocks in `waitid(WEXITED | WNOWAIT)`, which leaves the
/// exited child unreaped. The calling thread therefore still owns a live
/// process handle when the deadline passes and can kill it without racing a
/// recycled process id, then reap it. When the command pipes stderr, the
/// output is read after the child exited; only pipe a single-process command,
/// because a surviving grandchild would keep the pipe open.
pub(crate) fn run_bounded(
    command: &mut Command,
    timeout: Duration,
) -> Result<BoundedOutput, BoundedError> {
    let mut child = command.spawn().map_err(BoundedError::Spawn)?;
    let pid = Pid::from_child(&child);
    let (sender, receiver) = mpsc::channel();
    let watcher = thread::Builder::new()
        .name("pohunek-gui-bounded-wait".to_owned())
        .spawn(move || {
            let result = waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            );
            let _ = sender.send(result.map(|_| ()));
        });
    if let Err(source) = watcher {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BoundedError::Spawn(source));
    }
    match receiver.recv_timeout(timeout) {
        Ok(Ok(())) => {
            let mut stderr = Vec::new();
            if let Some(pipe) = child.stderr.take() {
                // The child exited, so the pipe reaches EOF; a read error
                // only loses the diagnostic.
                let _ = pipe.take(STDERR_CAPTURE_LIMIT).read_to_end(&mut stderr);
            }
            let status = child.wait().map_err(BoundedError::Wait)?;
            Ok(BoundedOutput { status, stderr })
        }
        Ok(Err(errno)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(BoundedError::Wait(io::Error::from(errno)))
        }
        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(BoundedError::Timeout)
        }
    }
}

/// Starts `program` detached from the GUI and reaps it on a helper thread.
///
/// The child gets its own process group and null stdio, so it survives GUI
/// exit and terminal signals aimed at the GUI's group. The `Child` is never
/// killed on drop; the helper thread's `wait` collects the exit status, so no
/// zombie outlives the process. The returned handle yields that status and may
/// be dropped.
///
/// # Errors
///
/// Returns the spawn error, or the error of spawning the helper thread.
pub(crate) fn spawn_detached(
    program: &OsStr,
    arguments: &[OsString],
) -> io::Result<JoinHandle<io::Result<ExitStatus>>> {
    let mut child = Command::new(program)
        .args(arguments)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::Builder::new()
        .name("pohunek-gui-reaper".to_owned())
        .spawn(move || child.wait())
}

/// Whether `path` names a script this module wrote.
#[cfg(test)]
fn is_script_name(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("attach-") && name.ends_with(".command"))
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
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod recorder");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = recorder(dir.path());
        let raw = OsStr::from_bytes(b"caf\xe9 '\xff'").to_owned();
        let argv = vec![recorder.into_os_string(), raw.clone()];
        let script = dir.path().join("script.command");
        fs::write(&script, command_script(&argv).expect("script")).expect("write script");

        assert_eq!(run_script(&script), vec![raw.as_bytes().to_vec()]);
    }

    #[test]
    fn script_deletes_itself_before_running_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
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
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).expect("chmod probe");
        let script = dir.path().join("script.command");
        let argv = vec![probe.into_os_string(), script.clone().into_os_string()];
        fs::write(&script, command_script(&argv).expect("script")).expect("write script");

        let status = Command::new("sh").arg(&script).status().expect("run");

        assert!(status.success());
        assert!(!script.exists(), "script removed itself");
        assert_eq!(fs::read_to_string(marker).expect("marker").trim(), "gone");
    }

    #[test]
    fn script_layout_is_shebang_self_delete_exec() {
        let script =
            command_script(&[OsString::from("/bin/echo"), OsString::from("it's")]).expect("script");
        assert_eq!(
            String::from_utf8(script).expect("utf8"),
            "#!/bin/sh\nrm -f -- \"$0\"\nexec '/bin/echo' 'it'\\''s'\n"
        );
    }

    #[test]
    fn nul_bytes_and_empty_programs_are_rejected() {
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
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake open");
        path
    }

    fn launcher(opener: PathBuf, script_dir: PathBuf, timeout: Duration) -> TerminalLauncher {
        TerminalLauncher::new(AttachTerminal::TerminalApp, opener, script_dir, timeout)
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("chmod target");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        // Leaves the script in place so its mode can be inspected.
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        launcher
            .launch(&[OsString::from("/bin/true")])
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
    fn every_launch_uses_a_fresh_script_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        launcher
            .launch(&[OsString::from("/bin/true")])
            .expect("first");
        launcher
            .launch(&[OsString::from("/bin/true")])
            .expect("second");

        assert_eq!(scripts_in(&script_dir).len(), 2);
    }

    #[test]
    fn a_group_writable_script_directory_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        fs::create_dir(&script_dir).expect("mkdir");
        fs::set_permissions(&script_dir, fs::Permissions::from_mode(0o770)).expect("chmod");
        let launcher = launcher(opener, script_dir, Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/bin/true")])
            .expect_err("unsafe directory");

        assert!(
            matches!(error, TerminalError::ScriptDirectory(_)),
            "{error}"
        );
        assert!(!record.exists(), "opener never ran");
    }

    #[test]
    fn a_symlinked_script_directory_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("mkdir");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("chmod");
        let link = dir.path().join("gui-attach");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let launcher = launcher(opener, link, Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/bin/true")])
            .expect_err("symlinked directory");

        assert!(
            matches!(error, TerminalError::ScriptDirectory(_)),
            "{error}"
        );
        assert!(fs::read_dir(&real).expect("real dir").next().is_none());
    }

    #[test]
    fn a_nul_byte_fails_before_any_file_or_process_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 0");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/bin/true"), OsString::from("a\0b")])
            .expect_err("nul");

        assert!(matches!(error, TerminalError::Script(ScriptError::NulByte)));
        assert!(!script_dir.exists());
        assert!(!record.exists());
    }

    #[test]
    fn a_failing_opener_is_a_typed_error_and_leaves_no_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        let opener = fake_open(dir.path(), &record, "exit 3");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_secs(30));

        let error = launcher
            .launch(&[OsString::from("/bin/true")])
            .expect_err("failing opener");

        assert!(
            matches!(error, TerminalError::OpenerFailed { .. }),
            "{error}"
        );
        assert!(scripts_in(&script_dir).is_empty());
    }

    #[test]
    fn a_missing_opener_is_a_typed_error_and_leaves_no_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(
            dir.path().join("no-such-open"),
            script_dir.clone(),
            Duration::from_secs(30),
        );

        let error = launcher
            .launch(&[OsString::from("/bin/true")])
            .expect_err("missing opener");

        assert!(
            matches!(error, TerminalError::OpenerSpawn { .. }),
            "{error}"
        );
        assert!(scripts_in(&script_dir).is_empty());
    }

    #[test]
    fn a_stalled_opener_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("open-argv");
        // A stopped process never exits by itself; only SIGKILL ends it.
        let opener = fake_open(dir.path(), &record, "kill -STOP $$");
        let script_dir = dir.path().join("gui-attach");
        let launcher = launcher(opener, script_dir.clone(), Duration::from_millis(200));

        let error = launcher
            .launch(&[OsString::from("/bin/true")])
            .expect_err("stalled opener");

        assert!(
            matches!(error, TerminalError::OpenerTimeout { .. }),
            "{error}"
        );
        assert!(scripts_in(&script_dir).is_empty());
    }

    #[test]
    fn spawn_detached_reaps_the_child_and_reports_its_status() {
        let handle = spawn_detached(
            OsStr::new("sh"),
            &[OsString::from("-c"), OsString::from("exit 7")],
        )
        .expect("spawn");

        let status = handle.join().expect("reaper thread").expect("wait");

        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn spawn_detached_puts_the_child_in_its_own_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("pgid");
        let handle = spawn_detached(
            OsStr::new("sh"),
            &[
                OsString::from("-c"),
                OsString::from(format!(
                    "ps -o pgid= -p $$ > '{}'; ps -o pid= -p $$ >> '{}'",
                    output.display(),
                    output.display()
                )),
            ],
        )
        .expect("spawn");
        handle.join().expect("reaper thread").expect("wait");

        let text = fs::read_to_string(&output).expect("ps output");
        let mut numbers = text.split_whitespace();
        let group = numbers.next().expect("pgid");
        let process = numbers.next().expect("pid");
        assert_eq!(group, process, "child leads its own process group");
    }

    #[test]
    fn spawn_detached_reports_a_missing_program() {
        let error = spawn_detached(OsStr::new("/nonexistent/pohunek-attach"), &[])
            .expect_err("missing program");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
