//! A release daemon running as a subprocess of the test, and the release CLI
//! that talks to it.
//!
//! Both are the layout's copies of the release binaries. The daemon runs in a
//! hermetic XDG environment and supervises its workers as direct children
//! (`POHUNEK_WORKER_LAUNCHER=subprocess`, the documented dev/test contract),
//! finding `pohunek-sessiond` and the trust anchor beside its own executable.
//! Every command goes through the CLI; the suite never links the daemon.

// Rust guideline compliant 2026-10-08

use std::fs::{self, File};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Output, Stdio};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use serde_json::Value;

use crate::release_bounded::{run as run_bounded, CLI_TIMEOUT};
use crate::release_layout::Layout;

/// Shell the daemon hands to terminal sessions; the sessions of this suite
/// launch agents directly, so it only has to exist.
const SHELL: &str = "/bin/sh";

/// Variable selecting direct-child worker supervision.
const LAUNCHER_VAR: &str = "POHUNEK_WORKER_LAUNCHER";

/// Value of [`LAUNCHER_VAR`] for supervision without a service manager.
const SUBPROCESS_LAUNCHER: &str = "subprocess";

/// Control socket of the daemon below the runtime directory.
const SOCKET: &str = "pohunek/daemon.sock";

/// Longest the daemon may take to exit after SIGTERM: it stops its workers and
/// flushes the event log (itself bounded at 5 s), so 60 s is generous while
/// still ending a daemon that ignores the signal.
const DAEMON_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Poll slice of `session wait`, in milliseconds. One slice is a bounded
/// request; the caller repeats it until its own condition holds.
const WAIT_SLICE_MS: &str = "8000";

/// Number of trailing log bytes printed when a run fails.
const LOG_TAIL_BYTES: usize = 4096;

/// Most hook lines of one agent transcript printed when a run fails.
const HOOK_LINES_MAX: usize = 20;

/// Longest hook line printed, in bytes.
const HOOK_LINE_MAX_BYTES: usize = 2048;

/// A running daemon, its environment and the CLI used against it.
pub(crate) struct Host {
    pub(crate) env: TestEnv,
    pub(crate) layout: Layout,
    daemon: Option<Child>,
    pub(crate) daemon_pid: u32,
}

impl Host {
    /// Starts the layout's daemon in `env`. `path_prefix` is put in front of
    /// the daemon's `PATH`, the way an installed upstream's directory is.
    ///
    /// # Panics
    ///
    /// Panics when the daemon cannot be spawned or exits before it accepts
    /// connections; the message carries the tail of its output.
    pub(crate) fn start(env: TestEnv, layout: Layout, path_prefix: Option<&Path>) -> Self {
        let out = File::create(env.root().join("daemon.out")).expect("create the daemon log");
        let err = out.try_clone().expect("duplicate the daemon log handle");
        let mut command = env.command(layout.path("pohunekd"));
        command
            .env("SHELL", SHELL)
            .env(LAUNCHER_VAR, SUBPROCESS_LAUNCHER)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        if let Some(prefix) = path_prefix {
            let inherited = env.environment().get(std::ffi::OsStr::new("PATH"));
            let mut dirs = vec![prefix.to_path_buf()];
            if let Some(inherited) = inherited {
                dirs.extend(std::env::split_paths(inherited));
            }
            command.env("PATH", std::env::join_paths(dirs).expect("join PATH"));
        }
        let child = command.spawn().expect("spawn the release daemon");
        let mut host = Self::adopt(env, layout, child);
        let socket = host.env.runtime_dir().join(SOCKET);
        poll_until("the daemon control socket accepts connections", || {
            if let Some(status) = host
                .daemon
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
            {
                panic!(
                    "the daemon exited before readiness ({status}):\n{}",
                    host.daemon_log()
                );
            }
            UnixStream::connect(&socket).ok().map(drop)
        });
        host
    }

    /// Takes ownership of an already spawned daemon process so that every
    /// exit path, including a panic, kills and reaps it.
    pub(crate) fn adopt(env: TestEnv, layout: Layout, daemon: Child) -> Self {
        let daemon_pid = daemon.id();
        Self {
            env,
            layout,
            daemon: Some(daemon),
            daemon_pid,
        }
    }

    /// The tail of the daemon's combined output.
    pub(crate) fn daemon_log(&self) -> String {
        let bytes = fs::read(self.env.root().join("daemon.out")).unwrap_or_default();
        let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }

    /// Best-effort failure diagnostics: never panics, ignores every error.
    fn dump_failure_diagnostics(&self) {
        self.dump_session_screens();
        dump_runtime_logs(self.env.root());
    }

    /// Prints the visible screen of up to five sessions, with diagnostics for each.
    fn dump_session_screens(&self) {
        let listed = match self.try_json(&["session", "list"]) {
            Ok(listed) => listed,
            Err(error) => {
                eprintln!("session list at the failure failed: {error}");
                return;
            }
        };
        let Some(sessions) = listed["ok"].as_array() else {
            eprintln!("session list at the failure: {listed}");
            return;
        };
        if sessions.is_empty() {
            eprintln!("session list at the failure: no sessions");
        }
        for id in sessions.iter().filter_map(|s| s["id"].as_str()).take(5) {
            match self.try_json(&["session", "screen", id]) {
                Ok(screen) => match screen["ok"]["visible_lines"].as_array() {
                    Some(lines) => {
                        eprintln!("screen of {id} at the failure:");
                        for line in lines.iter().filter_map(Value::as_str) {
                            eprintln!("{}", line.trim_end());
                        }
                    }
                    None => eprintln!("screen of {id} at the failure: {screen}"),
                },
                Err(error) => eprintln!("screen of {id} at the failure failed: {error}"),
            }
            match self.try_json(&["session", "inspect", id]) {
                Ok(inspect) => eprintln!("inspect of {id} at the failure: {inspect}"),
                Err(error) => eprintln!("inspect of {id} at the failure failed: {error}"),
            }
        }
    }

    /// Runs `pohunek <arguments> --json`, reporting errors descriptively.
    fn try_json(&self, arguments: &[&str]) -> Result<Value, String> {
        let mut command = self.env.command(self.layout.path("pohunek"));
        command.args(arguments).arg("--json");
        let output = run_bounded(command, &format!("pohunek {arguments:?}"), CLI_TIMEOUT)?;
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(format!("exit {code:?}: {}", tail(&output.stderr, 2048)));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("invalid JSON ({error}): {}", tail(&output.stdout, 2048)))
    }

    /// Runs the layout's `pohunek` with `arguments` in the hermetic environment.
    pub(crate) fn run(&self, arguments: &[&str]) -> Output {
        let mut command = self.env.command(self.layout.path("pohunek"));
        command.args(arguments);
        run_bounded(command, &format!("pohunek {arguments:?}"), CLI_TIMEOUT)
            .unwrap_or_else(|message| panic!("{message}"))
    }

    /// Runs `pohunek <arguments> --json` and returns the exit code and the
    /// single JSON document on stdout.
    pub(crate) fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self.run(&all);
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {} / {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().expect("exit code"), document)
    }

    /// Runs a command that must succeed and returns its `ok` payload.
    pub(crate) fn ok(&self, arguments: &[&str]) -> Value {
        let (code, document) = self.json(arguments);
        assert_eq!(code, 0, "{arguments:?} failed: {document}");
        document["ok"].clone()
    }

    /// Waits until `session wait` reports the session in `activity`.
    pub(crate) fn wait_activity(&self, id: &str, activity: &str) -> Value {
        poll_until(&format!("activity {activity} of {id}"), || {
            let waited = self.ok(&[
                "session",
                "wait",
                id,
                "--activity",
                activity,
                "--timeout-ms",
                WAIT_SLICE_MS,
            ]);
            (waited["reason"] == "activity_matched").then(|| waited["session"].clone())
        })
    }

    /// The session record `session inspect` reports.
    pub(crate) fn inspect(&self, id: &str) -> Value {
        self.ok(&["session", "inspect", id])
    }

    /// The visible rows of the session, trailing blanks removed.
    pub(crate) fn screen(&self, id: &str) -> Vec<String> {
        let screen = self.ok(&["session", "screen", id]);
        screen["visible_lines"]
            .as_array()
            .expect("visible lines")
            .iter()
            .map(|line| line.as_str().expect("line").trim_end().to_owned())
            .collect()
    }

    /// Waits until the visible screen holds a row containing `needle`.
    pub(crate) fn wait_screen_line(&self, id: &str, needle: &str) -> Vec<String> {
        poll_until(&format!("a screen line containing {needle:?}"), || {
            let rows = self.screen(id);
            rows.iter().any(|row| row.contains(needle)).then_some(rows)
        })
    }

    /// Stops the session and waits until its runtime is no longer live.
    pub(crate) fn stop_session(&self, id: &str) {
        self.ok(&["session", "stop", id]);
        poll_until(&format!("session {id} to stop"), || {
            (self.inspect(id)["runtime"]["state"] != "live").then_some(())
        });
    }

    /// Removes every session, then stops the daemon with SIGTERM and requires
    /// a successful exit.
    ///
    /// # Panics
    ///
    /// Panics when a session cannot be removed or the daemon exits with a
    /// failure status; the message carries the daemon log.
    pub(crate) fn shutdown(&mut self) {
        let listed = self.ok(&["session", "list"]);
        let ids: Vec<String> = listed
            .as_array()
            .expect("sessions")
            .iter()
            .map(|entry| entry["id"].as_str().expect("session id").to_owned())
            .collect();
        for id in &ids {
            self.ok(&["session", "rm", id]);
        }
        self.stop_daemon(DAEMON_EXIT_TIMEOUT);
    }

    /// Signals the daemon with SIGTERM and requires a successful exit within
    /// `deadline`.
    ///
    /// The child stays in `self.daemon` until it has exited, so a timeout or a
    /// failure status panics with the process still owned: the drop of the
    /// host kills and reaps it. A helper thread reaps the exit status, which
    /// keeps the wait bounded without polling.
    ///
    /// # Panics
    ///
    /// Panics when the signal fails, the daemon does not exit in time, or it
    /// exits with a failure status; the message carries the daemon log.
    pub(crate) fn stop_daemon(&mut self, deadline: std::time::Duration) {
        let pid = Pid::from_raw(i32::try_from(self.daemon_pid).expect("pid fits"));
        assert!(self.daemon.is_some(), "the daemon is running");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(nix::sys::wait::waitpid(pid, None));
        });
        kill(pid, Signal::SIGTERM).expect("signal the daemon");
        let status = rx.recv_timeout(deadline).unwrap_or_else(|_| {
            panic!(
                "the daemon did not exit within {deadline:?} of SIGTERM:\n{}",
                self.daemon_log()
            )
        });
        // The helper reaped the process; forgetting the handle keeps its drop
        // from waiting a second time.
        let child = self.daemon.take().expect("the daemon is running");
        drop(child);
        assert!(
            matches!(status, Ok(nix::sys::wait::WaitStatus::Exited(_, 0))),
            "the daemon did not exit cleanly ({status:?}):\n{}",
            self.daemon_log()
        );
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("daemon output before the failure:\n{}", self.daemon_log());
            self.dump_failure_diagnostics();
        }
        if let Some(mut child) = self.daemon.take() {
            // The daemon may have exited already; the process is not needed
            // past this point either way.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Returns the last `max` bytes of `bytes` as a lossy UTF-8 string.
fn tail(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Prints the last `LOG_TAIL_BYTES` of runtime log files found in the directory tree.
fn dump_runtime_logs(root: &Path) {
    let mut logs = Vec::new();
    collect_logs(root, 0, &mut logs);
    for path in logs {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        eprintln!("--- {}", path.display());
        eprintln!("{}", tail(&bytes, LOG_TAIL_BYTES));
        dump_hook_lines(&path, &bytes);
    }
}

/// Prints the hook entries of an agent transcript, wherever they sit in it.
///
/// The tail above can miss a session's first hook, and a hook the agent killed
/// at its timeout leaves no pohunek trace, so the transcript is the only record.
fn dump_hook_lines(path: &Path, bytes: &[u8]) {
    let in_transcripts = path.components().any(|part| part.as_os_str() == "projects");
    if !in_transcripts {
        return;
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().filter(|line| line.contains("hook")).collect();
    let omitted = lines.len().saturating_sub(HOOK_LINES_MAX);
    eprintln!(
        "hook lines of {}: {} shown, {omitted} later omitted",
        path.display(),
        lines.len() - omitted
    );
    for line in lines.iter().take(HOOK_LINES_MAX) {
        eprintln!("{}", truncate_at_char(line, HOOK_LINE_MAX_BYTES));
    }
}

/// `line` cut to at most `max` bytes on a character boundary.
fn truncate_at_char(line: &str, max: usize) -> &str {
    let mut end = line.len().min(max);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Recursively collects up to 30 log file paths, depth ≤ 8, skipping symlinks.
fn collect_logs(dir: &Path, depth: usize, logs: &mut Vec<std::path::PathBuf>) {
    if depth > 8 || logs.len() >= 30 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.file_type().is_file() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let is_log = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log"));
            let is_jsonl = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
            if (is_log || is_jsonl || name.contains("stderr")) && name != "daemon.out" {
                logs.push(path);
                if logs.len() >= 30 {
                    return;
                }
            }
        } else if metadata.file_type().is_dir() {
            collect_logs(&path, depth + 1, logs);
        }
    }
}
