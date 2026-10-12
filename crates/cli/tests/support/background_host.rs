//! A real `pohunekd` subprocess, the real CLI and a fake Claude agent, for the
//! #890 background-work scenarios.
//!
//! The daemon serves `claude` from the official Claude package, its only
//! source, which is installed into the daemon's plugin root before the daemon
//! starts (`crate::official_runtimes`, the daemon tests' catalog fixture); the
//! agent profile is pinned to it and the fake agent answers its version probe.
//!
//! The fake agent is the PTY child of its session. It runs the installed
//! Claude state hook as its direct child for every `hook:<action> <file>` line
//! the test sends through `session input`, so each hook reports the agent's own
//! pid and the hook JSON is the file the test wrote. Nothing is stubbed on the
//! hook side: the hook script, the worker and the daemon are the shipped ones.

use std::cell::RefCell;
use std::fs::{self, File};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::wait::{poll_until, HANG_GUARD};
use pohunek_test_support::workers::WorkerGuard;
use pohunek_test_support::{bin_exe, worker_binary};
use serde_json::Value;

/// Agent profile name the scenarios launch sessions with.
pub(crate) const PROFILE: &str = "bgclaude";

/// Control socket of the daemon below the runtime directory.
const SOCKET: &str = "pohunek/daemon.sock";

/// Owner-only directory mode for the config tree the daemon trusts.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Shell the daemon hands to terminal sessions; the fake agent is launched
/// directly, so the shell only has to exist.
const SHELL: &str = "/bin/sh";

/// Name of the daemon binary beside the CLI binary.
const DAEMON_NAME: &str = "pohunekd";

/// Name of the installed Claude state hook, as `integration install` writes it.
const STATE_HOOK: &str = "pohunek-agent-state.sh";

/// Subdirectory of the Claude config home that holds the installed hooks.
const HOOKS_DIR: &str = "hooks";

/// Longest the daemon may take to exit after SIGTERM before the test fails.
const DAEMON_EXIT_TIMEOUT: Duration = HANG_GUARD;

/// Number of trailing daemon output bytes printed when a run fails.
const LOG_TAIL_BYTES: usize = 4096;

/// The fake agent: runs one installed state hook per `hook:<action> <file>` line
/// and exits on `quit`. Hook inputs are read from the test's input directory,
/// not from the terminal, so the terminal never carries a payload.
///
/// [`fake_agent`] puts the answer to the package's version probe before it.
const FAKE_AGENT: &str = r#"# Fake Claude agent for the #890 background-work scenarios.
set -u
: "${FAKE_AGENT_HOOKS:?}" "${FAKE_AGENT_INPUTS:?}"
printf 'fake-agent ready\n'
while IFS= read -r line; do
    case "$line" in
        hook:*)
            request="${line#hook:}"
            action="${request%% *}"
            input="${request#* }"
            "$FAKE_AGENT_HOOKS/pohunek-agent-state.sh" "$action" <"$FAKE_AGENT_INPUTS/$input"
            printf 'fake-agent ran %s\n' "$action"
            ;;
        quit)
            exit 0
            ;;
    esac
done
"#;

/// The fake agent script: a shell script that answers the official Claude
/// package's version probe, then runs [`FAKE_AGENT`].
fn fake_agent() -> String {
    format!(
        "#!/bin/sh\n{}{FAKE_AGENT}",
        crate::official_runtimes::sh_version_answer("claude")
    )
}

/// Installs the official Codex and Claude packages into the plugin root the
/// daemon of `env` opens, before it starts.
fn install_official_packages(env: &TestEnv) {
    let scratch = env.root().join("official-packages");
    fs::create_dir_all(&scratch).expect("create the package scratch directory");
    let plugins = env
        .state_home()
        .join("pohunek")
        .join(pohunek_paths::PLUGINS_SUBDIR);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the package install")
        .block_on(crate::official_runtimes::OfficialCatalog::new().preinstall(&plugins, &scratch));
}

/// A running daemon with its hermetic environment, the CLI and the captured
/// output of every CLI call.
pub(crate) struct Host {
    // Held only for its drop: declared before `env`, so it reaps the daemon's
    // workers while the root still exists.
    _workers: WorkerGuard,
    pub(crate) env: TestEnv,
    daemon: Option<Child>,
    daemon_pid: u32,
    captured: RefCell<Vec<u8>>,
}

impl Host {
    /// Writes the fake agent and its profile, starts the daemon, and installs
    /// the Claude state hooks for the profile.
    ///
    /// # Panics
    ///
    /// Panics when the daemon cannot be spawned or does not accept connections,
    /// or when the hook installation reports a failure.
    pub(crate) fn start(env: TestEnv, inputs: &Path) -> Self {
        let agent = env.root().join("agents").join("fake-claude.sh");
        fs::create_dir_all(agent.parent().expect("agent parent")).expect("create agents dir");
        write_executable(&agent, fake_agent()).expect("write the fake agent");
        let claude_home = env.home().join(".claude");
        let path = env
            .environment()
            .get(std::ffi::OsStr::new("PATH"))
            .expect("the test environment carries PATH")
            .to_string_lossy()
            .into_owned();
        // The profile names only the fake agent and the hook location; the
        // hook finds python3 through PATH, which the agent environment needs.
        let profile = format!(
            "base = \"claude\"\n{}program = \"{}\"\nargs = []\n\n[env]\n\
             CLAUDE_CONFIG_DIR = \"{}\"\nFAKE_AGENT_HOOKS = \"{}\"\n\
             FAKE_AGENT_INPUTS = \"{}\"\nPATH = \"{path}\"\n\n\
             [input_rules]\nbracketed_paste = false\n",
            crate::official_runtimes::profile_pin("claude"),
            agent.display(),
            claude_home.display(),
            claude_home.join(HOOKS_DIR).display(),
            inputs.display(),
        );
        // The daemon refuses a config directory that is not private to the owner.
        let agents_dir = env.config_home().join("pohunek").join("agents");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(&agents_dir)
            .expect("create the agent profile directory");
        fs::write(agents_dir.join(format!("{PROFILE}.toml")), profile)
            .expect("write the agent profile");

        install_official_packages(&env);

        let workers = WorkerGuard::watch(env.root().to_path_buf());
        let out = File::create(env.root().join("daemon.out")).expect("create the daemon log");
        let err = out.try_clone().expect("duplicate the daemon log handle");
        let mut command = env.command(bin_exe("pohunek").with_file_name(DAEMON_NAME));
        command
            .env("SHELL", SHELL)
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        let child = command.spawn().expect("spawn pohunekd");
        let daemon_pid = child.id();
        let mut host = Self {
            _workers: workers,
            env,
            daemon: Some(child),
            daemon_pid,
            captured: RefCell::new(Vec::new()),
        };
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
        fs::create_dir_all(&claude_home).expect("create the Claude config home");
        let installed = host.ok_with_env(
            &[
                "integration",
                "install",
                "--agent",
                "claude",
                "--profile",
                PROFILE,
            ],
            &[("CLAUDE_CONFIG_DIR", claude_home.as_os_str())],
        );
        assert!(
            installed["failed"].as_array().is_none_or(Vec::is_empty),
            "the Claude hooks install: {installed}"
        );
        assert!(
            claude_home.join(HOOKS_DIR).join(STATE_HOOK).is_file(),
            "the state hook is installed under CLAUDE_CONFIG_DIR"
        );
        host
    }

    /// The tail of the daemon's combined output.
    pub(crate) fn daemon_log(&self) -> String {
        let bytes = fs::read(self.env.root().join("daemon.out")).unwrap_or_default();
        let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }

    /// Runs the CLI and keeps its output for the sink scan.
    pub(crate) fn run(&self, arguments: &[&str]) -> Output {
        self.run_with_env(arguments, &[])
    }

    /// Runs the CLI with extra variables and keeps its output for the sink scan.
    pub(crate) fn run_with_env(
        &self,
        arguments: &[&str],
        extra: &[(&str, &std::ffi::OsStr)],
    ) -> Output {
        let mut command = self.env.command(bin_exe("pohunek"));
        command.args(arguments).envs(extra.iter().copied());
        let output = bounded(command, &format!("pohunek {arguments:?}"));
        let mut captured = self.captured.borrow_mut();
        captured.extend_from_slice(&output.stdout);
        captured.extend_from_slice(&output.stderr);
        output
    }

    /// Runs `pohunek <arguments> --json` and returns the exit code and document.
    pub(crate) fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self.run(&all);
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        (output.status.code().expect("exit code"), document)
    }

    /// Runs a JSON command that must succeed and returns its `ok` payload.
    pub(crate) fn ok(&self, arguments: &[&str]) -> Value {
        self.ok_with_env(arguments, &[])
    }

    fn ok_with_env(&self, arguments: &[&str], extra: &[(&str, &std::ffi::OsStr)]) -> Value {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self.run_with_env(&all, extra);
        let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        assert_eq!(output.status.code(), Some(0), "{all:?} failed: {document}");
        document["ok"].clone()
    }

    /// Creates a session on the scenario profile and returns its id.
    pub(crate) fn create_session(&self, name: &str) -> String {
        let cwd = self.env.cwd().display().to_string();
        let created = self.ok(&[
            "session", "new", "--agent", PROFILE, "--cwd", &cwd, "--name", name,
        ]);
        created["session"]["id"]
            .as_str()
            .or_else(|| created["id"].as_str())
            .expect("created session id")
            .to_owned()
    }

    /// Waits until the agent in `id` prints `needle` on its screen.
    pub(crate) fn wait_screen_line(&self, id: &str, needle: &str) {
        poll_until(&format!("a screen line containing {needle:?}"), || {
            let screen = self.ok(&["session", "screen", id]);
            screen["visible_lines"]
                .as_array()
                .expect("visible lines")
                .iter()
                .any(|line| line.as_str().is_some_and(|line| line.contains(needle)))
                .then_some(())
        });
    }

    /// Makes the fake agent run one hook: `action` with the payload file `input`.
    pub(crate) fn run_hook(&self, id: &str, action: &str, input: &str) {
        let line = format!("hook:{action} {input}");
        self.ok(&["session", "input", id, &line]);
        self.wait_screen_line(id, &format!("fake-agent ran {action}"));
    }

    /// Waits until `session inspect` reports a background snapshot of `count` items.
    pub(crate) fn wait_background(&self, id: &str, count: usize) -> Value {
        poll_until(&format!("a background snapshot of {count} items"), || {
            let (code, document) = self.json(&["session", "inspect", id]);
            assert_eq!(code, 0, "{document}");
            let items = document["ok"]["background"]["items"].as_array()?;
            (items.len() == count).then(|| document["ok"].clone())
        })
    }

    /// Waits until `session inspect` lists `count` subagents.
    pub(crate) fn wait_subagents(&self, id: &str, count: usize) {
        poll_until(&format!("{count} subagent rows"), || {
            let session = self.inspect(id);
            (session["subagents"].as_array()?.len() == count).then_some(())
        });
    }

    /// The session record `session inspect --json` reports.
    pub(crate) fn inspect(&self, id: &str) -> Value {
        self.ok(&["session", "inspect", id])
    }

    /// Stops the session and waits until its runtime is no longer live.
    pub(crate) fn stop_session(&self, id: &str) {
        self.ok(&["session", "stop", id]);
        poll_until(&format!("session {id} to stop"), || {
            (self.inspect(id)["runtime"]["state"] != "live").then_some(())
        });
    }

    /// Stops the daemon with SIGTERM and requires a clean exit, so its logs and
    /// the workers' journals are flushed before the sink scan.
    ///
    /// # Panics
    ///
    /// Panics when the signal fails, the daemon does not exit in time, or it
    /// exits with a failure status; the message carries the daemon log.
    pub(crate) fn stop_daemon(&mut self) {
        let pid = Pid::from_raw(i32::try_from(self.daemon_pid).expect("pid fits"));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(nix::sys::wait::waitpid(pid, None));
        });
        kill(pid, Signal::SIGTERM).expect("signal the daemon");
        let status = rx.recv_timeout(DAEMON_EXIT_TIMEOUT).unwrap_or_else(|_| {
            panic!(
                "the daemon did not exit within {DAEMON_EXIT_TIMEOUT:?} of SIGTERM:\n{}",
                self.daemon_log()
            )
        });
        // The helper thread reaped the process; forget the handle so Drop does
        // not wait on it again.
        drop(self.daemon.take());
        assert!(
            matches!(status, Ok(WaitStatus::Exited(_, 0))),
            "the daemon did not exit cleanly ({status:?}):\n{}",
            self.daemon_log()
        );
    }

    /// Every file under the scanned root and every CLI output this host captured,
    /// returned as the needles each one contains, one entry per match.
    pub(crate) fn leaks(&self, needles: &[&str]) -> Vec<String> {
        let mut found = Vec::new();
        let mut files = Vec::new();
        collect_files(self.env.root(), &mut files);
        for file in files {
            // A regular file that cannot be read fails the scan: skipping it
            // would hide a sink from the check.
            let bytes = fs::read(&file)
                .unwrap_or_else(|error| panic!("sink {} unreadable: {error}", file.display()));
            for needle in needles {
                if contains(&bytes, needle.as_bytes()) {
                    found.push(format!("{} contains {needle:?}", file.display()));
                }
            }
        }
        let captured = self.captured.borrow();
        for needle in needles {
            if contains(&captured, needle.as_bytes()) {
                found.push(format!("CLI output contains {needle:?}"));
            }
        }
        found
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("daemon output before the failure:\n{}", self.daemon_log());
        }
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Runs `command` to completion, failing with `what` after [`HANG_GUARD`].
fn bounded(mut command: Command, what: &str) -> Output {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    rx.recv_timeout(HANG_GUARD)
        .unwrap_or_else(|_| panic!("{what} did not finish within {HANG_GUARD:?}"))
        .unwrap_or_else(|error| panic!("{what} could not run: {error}"))
}

/// Every regular file below `dir`, without following symbolic links.
///
/// Symbolic links are neither followed nor scanned: `DirEntry::file_type`
/// reports the link itself, so a link cannot pull in a file outside the root.
/// Sockets and other special files hold no sink content and are skipped. A
/// directory that cannot be listed fails the scan, since its files would
/// otherwise drop out unseen.
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("sink directory {} unreadable: {error}", dir.display()));
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|error| panic!("sink directory {} unreadable: {error}", dir.display()));
        let path = entry.path();
        let kind = entry
            .file_type()
            .unwrap_or_else(|error| panic!("sink {} unreadable: {error}", path.display()));
        if kind.is_dir() {
            collect_files(&path, files);
        } else if kind.is_file() {
            files.push(path);
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
