//! End-to-end contracts of `pohunek workflow run`: the exact child
//! environment, the session origin pair, refusals before anything starts,
//! deadlines, output caps, interruption, redaction, the context, the run log
//! and the owner knobs.
//!
//! Every scenario runs the real `pohunek` binary in a hermetic environment.
//! Plugins are built in the test with `package::build_archive` and installed
//! with `pohunek workflow install --yes`. Plugin programs use absolute paths
//! and shell builtins only, because the runner gives them no `PATH`. The
//! origin scenario adds a real `pohunekd` with real `pohunek-sessiond`
//! workers and runs the action from inside a managed session.

// Rust guideline compliant 2026-10-13

#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use pohunek_test_support::workers::WorkerGuard;
use pohunek_test_support::{bin_exe, worker_binary};
use serde_json::Value;

/// Plugin id of the fixture manifest.
const PLUGIN_ID: &str = "acme.work";

/// Plugin version of the fixture manifest.
const PLUGIN_VERSION: &str = "1.2.0";

/// A manifest argv value that must never leave the child.
const ARGV_SENTINEL: &str = "argv-sentinel-5d1e";

/// A command-line argument value that must never leave the child.
const PASS_SENTINEL: &str = "pass-sentinel-93b0";

/// An output value written past the cap, so it must reach nothing.
const OUTPUT_SENTINEL: &str = "output-sentinel-c47a";

/// A parent environment value that must never reach the child.
const SECRET_SENTINEL: &str = "secret-sentinel-0e2f";

/// Output cap the `talk` action declares, in bytes.
const TALK_CAP: u64 = 4096;

/// Exit status of a run stopped at its deadline (`docs/public-api.md`).
const TIMEOUT_EXIT: i32 = 124;

/// Exit status of a run stopped at its output cap (`docs/public-api.md`).
const OUTPUT_LIMIT_EXIT: i32 = 125;

/// Exit status of a run stopped because the CLI received `SIGINT`.
const SIGINT_EXIT: i32 = 128 + libc::SIGINT;

/// Grace between `SIGTERM` and `SIGKILL` in the scenarios that stop a run;
/// short, so a scenario that waits for the forced kill stays quick.
const TEST_GRACE_KNOBS: &str = "[limits]\nkill_grace_ms = 300\naction_timeout_default_ms = 5000\n";

/// Every action the fixture plugin declares.
///
/// `sleepy` and `stubborn` leave a grandchild running and record both PIDs;
/// `stubborn` ignores `SIGTERM`. `flood` writes forever and ignores
/// `SIGPIPE`, so only a group kill ends it. `talk` writes exactly its cap of
/// filler to each stream, then the output sentinel. `long` declares a
/// timeout one above the default ceiling.
fn manifest() -> String {
    format!(
        r#"schema = 1

[plugin]
id = "{PLUGIN_ID}"
version = "{PLUGIN_VERSION}"
description = "Workflow run scenarios"

[actions.env]
argv = ["/usr/bin/env"]

[actions.mark]
argv = ["bin/mark", "{ARGV_SENTINEL}"]
args = "passthrough"

[actions.fixed]
argv = ["bin/mark"]
timeout_ms = 600000

[actions.long]
argv = ["bin/mark"]
timeout_ms = 600001

[actions.sleepy]
argv = ["bin/sleepy"]

[actions.stubborn]
argv = ["bin/stubborn"]
timeout_ms = 5000

[actions.flood]
argv = ["bin/flood"]

[actions.talk]
argv = ["bin/talk", "{ARGV_SENTINEL}"]
args = "passthrough"
max_output_bytes = {TALK_CAP}

[actions.burst]
argv = ["bin/burst"]

[actions.escape]
argv = ["bin/escape"]

[actions.stop_self]
argv = ["bin/stop-self"]
args = "passthrough"
"#
    )
}

/// Records the PIDs of the plugin shell and its grandchild, then waits.
///
/// The grandchild sleeps far longer than any scenario, so only the runner's
/// group kill ends it.
// timing-allowed: #148 a long-running grandchild is the subject of the kill scenarios.
const SLEEPY: &str = r#"#!/bin/sh
/bin/sleep 600 &
printf '%s\n' "$!" > "$POHUNEK_PLUGIN_STATE_DIR/grandchild.pid"
printf '%s\n' "$$" > "$POHUNEK_PLUGIN_STATE_DIR/child.pid"
wait
"#;

/// [`SLEEPY`] with `SIGTERM` ignored by the shell and its grandchild.
// timing-allowed: #148 a long-running grandchild is the subject of the kill scenarios.
const STUBBORN: &str = r#"#!/bin/sh
trap '' TERM
/bin/sleep 600 &
printf '%s\n' "$!" > "$POHUNEK_PLUGIN_STATE_DIR/grandchild.pid"
printf '%s\n' "$$" > "$POHUNEK_PLUGIN_STATE_DIR/child.pid"
wait
"#;

/// Writes filler forever, ignoring `SIGPIPE` and discarding the write errors
/// that follow, with a sleeping grandchild.
// timing-allowed: #148 a long-running grandchild is the subject of the kill scenarios.
const FLOOD: &str = r#"#!/bin/sh
trap '' PIPE
/bin/sleep 600 &
printf '%s\n' "$!" > "$POHUNEK_PLUGIN_STATE_DIR/grandchild.pid"
while :; do printf '%s\n' 'filler-filler-filler-filler-filler-filler-filler-filler' 2>/dev/null; done
"#;

fn talk() -> String {
    // 64 lines of 63 characters plus a newline: exactly the 4096-byte cap.
    let line = "t".repeat(63);
    format!(
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 64 ]; do printf '%s\\n' '{line}'; \
         printf '%s\\n' '{line}' >&2; i=$((i + 1)); done\n\
         printf '%s\\n' '{OUTPUT_SENTINEL}'\nprintf '%s\\n' '{OUTPUT_SENTINEL}' >&2\n"
    )
}

/// Asks the daemon to stop the session named by its first argument.
///
/// The session id arrives as an argument, not through the origin markers, so
/// only the forwarded markers can make the daemon refuse the stop.
const STOP_SELF: &str = r#"#!/bin/sh
"$POHUNEK_CLI" session stop "$1" --json > "$POHUNEK_PLUGIN_STATE_DIR/stop.json" 2> "$POHUNEK_PLUGIN_STATE_DIR/stop.err"
"#;

/// Lines and line length of [`burst`]'s output: 96 lines of 127 bytes plus a
/// newline, 12 KiB, which fits the smallest pipe buffer of a supported host
/// (16 KiB on macOS), so the action can exit while its output still waits.
const BURST_LINES: usize = 96;
/// Bytes per [`burst`] line before the newline.
const BURST_LINE_BYTES: usize = 127;

/// Records its PID, writes [`burst_output`] to stdout and exits 0.
fn burst() -> String {
    let line = "b".repeat(BURST_LINE_BYTES);
    format!(
        "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$POHUNEK_PLUGIN_STATE_DIR/child.pid\"\ni=0\n\
         while [ \"$i\" -lt {BURST_LINES} ]; do printf '%s\\n' '{line}'; i=$((i + 1)); done\n"
    )
}

/// What [`burst`] writes to stdout.
fn burst_output() -> String {
    format!("{}\n", "b".repeat(BURST_LINE_BYTES)).repeat(BURST_LINES)
}

/// How long the slow consumer leaves the CLI's stdout unread after the action
/// exited: three times the runner's one-second idle bound for output pipes,
/// so a runner that counted the consumer's delay would have given up.
const CONSUMER_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// Leaves behind a process in a session of its own that keeps stdout and
/// stderr open and writes a byte to stdout five times a second, then exits 0
/// once that process has left the group, so the group kill cannot reach it.
// timing-allowed: #148 a steadily writing escaped process is the subject of the cut scenario.
const ESCAPE: &str = r#"#!/bin/sh
/usr/bin/setsid /bin/sh -c ': > "$POHUNEK_PLUGIN_STATE_DIR/escaped"; while :; do printf x; /bin/sleep 0.2; done' &
printf '%s\n' "$!" > "$POHUNEK_PLUGIN_STATE_DIR/escapee.pid"
while [ ! -e "$POHUNEK_PLUGIN_STATE_DIR/escaped" ]; do :; done
"#;

/// Creates a marker file, proving the program ran.
const MARK: &str = "#!/bin/sh\n: > \"$POHUNEK_PLUGIN_STATE_DIR/spawned\"\n";

fn entry(path: &str, contents: &str, executable: bool) -> ArchiveEntry {
    ArchiveEntry {
        path: path.to_owned(),
        contents: contents.as_bytes().to_vec(),
        executable,
    }
}

fn plugin_files() -> Vec<ArchiveEntry> {
    vec![
        entry("workflow.toml", &manifest(), false),
        entry("bin/mark", MARK, true),
        entry("bin/sleepy", SLEEPY, true),
        entry("bin/stubborn", STUBBORN, true),
        entry("bin/flood", FLOOD, true),
        entry("bin/talk", &talk(), true),
        entry("bin/stop-self", STOP_SELF, true),
        entry("bin/burst", &burst(), true),
        entry("bin/escape", ESCAPE, true),
    ]
}

/// One hermetic environment with the fixture plugin installed.
struct Fixture {
    env: TestEnv,
    digest: PackageDigest,
}

impl Fixture {
    fn new() -> Self {
        Self::in_env(TestEnv::new().expect("create the hermetic test environment"))
    }

    /// Installs the fixture plugin into `env`.
    fn in_env(env: TestEnv) -> Self {
        let bytes = build_archive(&plugin_files(), &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let archive = env.root().join("work.tar.zst");
        fs::write(&archive, bytes).expect("write the archive");
        let fixture = Self { env, digest };
        let mut install = fixture.command(&[
            "workflow",
            "install",
            archive.to_str().expect("UTF-8 path"),
            "--sha256",
            fixture.digest.as_str(),
            "--yes",
            "--json",
        ]);
        let output = install.output().expect("run the install");
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        fixture
    }

    /// `pohunek <arguments>` in the hermetic environment.
    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = self.env.command(bin_exe("pohunek"));
        command.args(arguments);
        command
    }

    /// `pohunek workflow run acme.work <action> [-- <args>]` with `extra`
    /// variables on top of the hermetic environment.
    fn run_with(&self, action: &str, args: &[&str], extra: &[(&str, &OsStr)]) -> Output {
        let mut arguments = vec!["workflow", "run", PLUGIN_ID, action];
        if !args.is_empty() {
            arguments.push("--");
            arguments.extend_from_slice(args);
        }
        let mut command = self.command(&arguments);
        command.envs(extra.iter().copied());
        command.output().expect("run the pohunek binary")
    }

    fn run(&self, action: &str) -> Output {
        self.run_with(action, &[], &[])
    }

    /// Writes `text` as the owner knobs file with mode 0600.
    fn knobs(&self, text: &str) {
        let dir = self.env.config_home().join("pohunek");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create the pohunek config directory");
        let path = dir.join("workflow.toml");
        fs::write(&path, text).expect("write workflow.toml");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod knobs");
    }

    fn state_dir(&self) -> PathBuf {
        self.env
            .state_home()
            .join("pohunek/workflow-plugin-state")
            .join(PLUGIN_ID)
    }

    fn config_dir(&self) -> PathBuf {
        self.env
            .config_home()
            .join("pohunek/workflow-plugin-config")
            .join(PLUGIN_ID)
    }

    fn files_dir(&self) -> PathBuf {
        self.env
            .state_home()
            .join("pohunek/workflow-plugins/packages")
            .join(self.digest.as_str().trim_start_matches("sha256:"))
            .join("files")
    }

    /// Every file of the run log family, active and rotated.
    fn run_log_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.state_dir().join("logs")) else {
            return Vec::new();
        };
        entries
            .map(|entry| entry.expect("read a log entry").path())
            .filter(|path| {
                let family = path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with("runs.jsonl"));
                family && path.extension() != Some(OsStr::new("lock"))
            })
            .collect()
    }

    /// The records of the active run log, oldest first.
    fn records(&self) -> Vec<Value> {
        let text = fs::read_to_string(self.state_dir().join("logs/runs.jsonl")).unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str(line).expect("a run record is one JSON line"))
            .collect()
    }

    /// The newest run record.
    fn last_record(&self) -> Value {
        self.records().pop().expect("a run record")
    }

    /// Whether `session inspect` reports the session's runtime as live.
    fn session_live(&self, session: &str) -> bool {
        let inspected = self
            .command(&["session", "inspect", session, "--json"])
            .output()
            .expect("inspect the session");
        let document: Value = serde_json::from_slice(&inspected.stdout).expect("inspect JSON");
        document["ok"]["runtime"]["state"] == "live"
    }

    fn marker(&self) -> PathBuf {
        self.state_dir().join("spawned")
    }

    /// Reads a PID file of a finished run.
    fn written_pid(&self, name: &str) -> Pid {
        let path = self.state_dir().join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "the plugin did not write {name} before its run ended ({error}); \
                 the host may be too slow for the scenario's deadline"
            )
        });
        Pid::from_raw(
            text.trim_end()
                .parse()
                .unwrap_or_else(|error| panic!("{name} holds a PID: {error}")),
        )
    }

    /// Reads a PID file the plugin of a running action writes, waiting until
    /// it is complete.
    fn pid(&self, name: &str) -> Pid {
        let path = self.state_dir().join(name);
        poll_until(&format!("the plugin to write {name}"), || {
            let text = fs::read_to_string(&path).ok()?;
            text.strip_suffix('\n')?.parse().ok().map(Pid::from_raw)
        })
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parses the `KEY=VALUE` lines `/usr/bin/env` printed.
fn environment(output: &Output) -> BTreeMap<String, String> {
    String::from_utf8(output.stdout.clone())
        .expect("UTF-8 environment")
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('=').expect("KEY=VALUE");
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

/// Waits until no process has `pid`: the group kill reached it and its
/// parent reaped it.
fn wait_gone(pid: Pid, what: &str) {
    poll_until(&format!("{what} ({pid}) to be gone"), || {
        (kill(pid, None) == Err(nix::errno::Errno::ESRCH)).then_some(())
    });
}

/// The exact environment the runner must give the plugin, without the
/// origin pair: the bootstrap roots and the plugin variables.
fn expected_keys() -> Vec<&'static str> {
    vec![
        "HOME",
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "POHUNEK_PLUGIN_ID",
        "POHUNEK_PLUGIN_VERSION",
        "POHUNEK_PLUGIN_DIGEST",
        "POHUNEK_PLUGIN_CONFIG_DIR",
        "POHUNEK_PLUGIN_STATE_DIR",
        "POHUNEK_PLUGIN_CONTEXT",
        "POHUNEK_CLI",
    ]
}

#[test]
fn the_child_gets_exactly_the_allowlist_and_the_complete_origin_pair() {
    let fixture = Fixture::new();
    let hostile: [(&str, &OsStr); 7] = [
        (
            "POHUNEK_SOCKET_PATH",
            OsStr::new("/nonexistent/worker.sock"),
        ),
        ("POHUNEK_WORKER_ID", OsStr::new("w-fake")),
        ("GH_TOKEN", OsStr::new(SECRET_SENTINEL)),
        ("SECRET_SENTINEL", OsStr::new(SECRET_SENTINEL)),
        ("PATH", OsStr::new("/usr/bin:/bin")),
        ("POHUNEK_SESSION_ID", OsStr::new("s-origin-1")),
        ("POHUNEK_DAEMON_ID", OsStr::new("d-origin-1")),
    ];

    let output = fixture.run_with("env", &[], &hostile);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let child = environment(&output);
    let mut keys = expected_keys();
    keys.extend(["POHUNEK_SESSION_ID", "POHUNEK_DAEMON_ID"]);
    keys.sort_unstable();
    assert_eq!(child.keys().map(String::as_str).collect::<Vec<_>>(), keys);
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(SECRET_SENTINEL),
        "a parent secret reached the child"
    );
    assert_eq!(child["POHUNEK_SESSION_ID"], "s-origin-1");
    assert_eq!(child["POHUNEK_DAEMON_ID"], "d-origin-1");
    let env = &fixture.env;
    for (key, path) in [
        ("HOME", env.home()),
        ("XDG_RUNTIME_DIR", env.runtime_dir()),
        ("XDG_STATE_HOME", env.state_home()),
        ("XDG_CONFIG_HOME", env.config_home()),
        ("XDG_DATA_HOME", env.data_home()),
        ("XDG_CACHE_HOME", env.cache_home()),
    ] {
        assert_eq!(Path::new(&child[key]), path, "{key}");
    }
    assert_eq!(child["POHUNEK_PLUGIN_ID"], PLUGIN_ID);
    assert_eq!(child["POHUNEK_PLUGIN_VERSION"], PLUGIN_VERSION);
    assert_eq!(child["POHUNEK_PLUGIN_DIGEST"], fixture.digest.as_str());
    assert_eq!(
        Path::new(&child["POHUNEK_PLUGIN_STATE_DIR"]),
        fixture.state_dir()
    );
    assert_eq!(
        Path::new(&child["POHUNEK_PLUGIN_CONFIG_DIR"]),
        fixture.config_dir()
    );
    assert_eq!(
        Path::new(&child["POHUNEK_CLI"]),
        fs::canonicalize(bin_exe("pohunek")).expect("canonical CLI path")
    );

    // Without markers the pair is absent; nothing else changes.
    let output = fixture.run_with("env", &[], &hostile[..5]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let child = environment(&output);
    let mut keys = expected_keys();
    keys.sort_unstable();
    assert_eq!(child.keys().map(String::as_str).collect::<Vec<_>>(), keys);
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "ok", "{record}");
    assert_eq!(record["exit_code"], 0, "{record}");
}

#[test]
fn a_single_origin_marker_is_refused_before_anything_starts() {
    let fixture = Fixture::new();
    for marker in ["POHUNEK_SESSION_ID", "POHUNEK_DAEMON_ID"] {
        let output = fixture.run_with("mark", &[], &[(marker, OsStr::new("lonely"))]);
        assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
        assert!(
            stderr(&output).contains("exactly one of POHUNEK_SESSION_ID and POHUNEK_DAEMON_ID"),
            "{}",
            describe(&output)
        );
        assert!(!fixture.marker().exists(), "{marker}: the plugin ran");
        let record = fixture.last_record();
        assert_eq!(record["outcome"], "refused", "{record}");
        assert_eq!(
            record["error_code"], "workflow_origin_incomplete",
            "{record}"
        );
    }
}

#[test]
fn a_deadline_stops_the_group_with_sigterm_and_a_stubborn_one_with_sigkill() {
    let fixture = Fixture::new();
    fixture.knobs(TEST_GRACE_KNOBS);

    // `sleepy` declares no timeout, so the owner's default deadline applies.
    for (action, signal) in [("sleepy", libc::SIGTERM), ("stubborn", libc::SIGKILL)] {
        let output = fixture.run(action);
        assert_eq!(
            output.status.code(),
            Some(TIMEOUT_EXIT),
            "{action}: {}",
            describe(&output)
        );
        assert!(
            stderr(&output).contains("did not finish within 5000 ms"),
            "{action}: {}",
            describe(&output)
        );
        // The run is over, so the PID files are complete or will never be.
        wait_gone(fixture.written_pid("child.pid"), "the plugin shell");
        wait_gone(fixture.written_pid("grandchild.pid"), "the grandchild");
        let record = fixture.last_record();
        assert_eq!(record["outcome"], "timeout", "{action}: {record}");
        assert_eq!(record["error_code"], "workflow_action_timeout", "{record}");
        assert_eq!(record["signal"], signal, "{action}: {record}");
        for name in ["child.pid", "grandchild.pid"] {
            fs::remove_file(fixture.state_dir().join(name)).expect("reset the PID file");
        }
    }
}

#[test]
fn an_output_cap_stops_the_group_early_and_records_only_counts() {
    let fixture = Fixture::new();
    // A one-day deadline: only an early stop at the cap ends the run before
    // the hang guard. The outcome alone cannot show it, because an output
    // limit outranks a timeout.
    fixture.knobs(
        "[limits]\nmax_stdout_bytes = 4096\nkill_grace_ms = 300\n\
         action_timeout_max_ms = 86400000\naction_timeout_default_ms = 86400000\n",
    );

    let mut cli = fixture
        .command(&["workflow", "run", PLUGIN_ID, "flood"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    // Both streams stay far below a pipe buffer, so the CLI never blocks on them.
    poll_until("the CLI to stop the flood at its cap", || {
        cli.try_wait().expect("poll the CLI").map(drop)
    });
    let output = cli.wait_with_output().expect("collect the CLI output");
    assert_eq!(
        output.status.code(),
        Some(OUTPUT_LIMIT_EXIT),
        "{}",
        describe(&output)
    );
    assert_eq!(output.stdout.len(), 4096, "only the cap reached stdout");
    assert!(
        stderr(&output).contains("wrote more than 4096 bytes to stdout"),
        "{}",
        describe(&output)
    );
    wait_gone(fixture.written_pid("grandchild.pid"), "the grandchild");
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "output_limit", "{record}");
    assert_eq!(record["error_code"], "workflow_output_limit", "{record}");
    assert_eq!(record["stdout_bytes"], 4096, "{record}");
    assert_eq!(record["stderr_bytes"], 0, "{record}");
    let log = fs::read_to_string(fixture.state_dir().join("logs/runs.jsonl")).expect("log");
    assert!(!log.contains("filler"), "output bytes reached the run log");
}

#[test]
fn sentinels_in_argv_and_past_the_cap_appear_nowhere() {
    let fixture = Fixture::new();

    let output = fixture.run_with("talk", &[PASS_SENTINEL], &[]);
    assert_eq!(
        output.status.code(),
        Some(OUTPUT_LIMIT_EXIT),
        "{}",
        describe(&output)
    );
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "output_limit", "{record}");
    assert_eq!(record["arg_count"], 1, "{record}");
    // `fixed` accepts no extra arguments; the refusal counts them and names
    // no value.
    let refused = fixture.run_with("fixed", &[PASS_SENTINEL], &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    assert!(
        stderr(&refused).contains("accepts no extra arguments; 1 were given"),
        "{}",
        describe(&refused)
    );
    assert_eq!(
        fixture.last_record()["error_code"],
        "workflow_action_args_refused"
    );

    let mut sinks = vec![
        ("stdout".to_owned(), output.stdout.clone()),
        ("stderr".to_owned(), output.stderr.clone()),
        ("refusal stdout".to_owned(), refused.stdout.clone()),
        ("refusal stderr".to_owned(), refused.stderr.clone()),
    ];
    for path in fixture.run_log_files() {
        sinks.push((
            path.display().to_string(),
            fs::read(&path).expect("read a run log file"),
        ));
    }
    assert!(!fixture.run_log_files().is_empty(), "the run log exists");
    for (sink, bytes) in &sinks {
        let text = String::from_utf8_lossy(bytes);
        for sentinel in [ARGV_SENTINEL, PASS_SENTINEL, OUTPUT_SENTINEL] {
            assert!(!text.contains(sentinel), "{sink} contains {sentinel}");
        }
    }
}

#[test]
fn the_context_has_the_documented_schema_and_an_oversized_one_is_refused() {
    let fixture = Fixture::new();

    let output = fixture.run("env");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let context: Value =
        serde_json::from_str(&environment(&output)["POHUNEK_PLUGIN_CONTEXT"]).expect("JSON");
    let record = fixture.last_record();
    let run_id = record["run_id"].as_str().expect("run id");
    assert_eq!(run_id.len(), 26, "a ULID: {run_id}");
    let keys = |value: &Value| {
        value
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(&context),
        ["cli", "invocation", "output", "plugin", "schema"]
    );
    assert_eq!(context["schema"], 1);
    assert_eq!(
        context["plugin"],
        serde_json::json!({"id": PLUGIN_ID, "version": PLUGIN_VERSION, "digest": fixture.digest.as_str()})
    );
    assert_eq!(
        context["invocation"],
        serde_json::json!({"kind": "action", "name": "env", "run_id": run_id})
    );
    assert_eq!(context["output"], "text");
    assert_eq!(keys(&context["cli"]), ["protocol", "version"]);
    assert_eq!(context["cli"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(keys(&context["cli"]["protocol"]), ["maximum", "minimum"]);
    assert!(context["cli"]["protocol"]["maximum"].is_u64(), "{context}");

    // The context is one compact line; a cap below its size refuses the run.
    fixture.knobs("[limits]\nmax_context_bytes = 64\n");
    let output = fixture.run("mark");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output).contains("above limits.max_context_bytes 64"),
        "{}",
        describe(&output)
    );
    assert!(!fixture.marker().exists(), "the plugin ran");
    assert_eq!(
        fixture.last_record()["error_code"],
        "workflow_context_too_large"
    );
}

#[test]
fn a_tampered_plugin_is_refused_and_nothing_runs() {
    let fixture = Fixture::new();
    let program = fixture.files_dir().join("bin/mark");
    let tampered = "#!/bin/sh\n: > \"$POHUNEK_PLUGIN_STATE_DIR/tampered-ran\"\n";
    fs::write(&program, tampered).expect("tamper with the installed program");

    let output = fixture.run("mark");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output).contains("fail verification"),
        "{}",
        describe(&output)
    );
    assert!(!fixture.marker().exists());
    assert!(!fixture.state_dir().join("tampered-ran").exists());
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["error_code"], "workflow_root_invalid", "{record}");
    assert!(record.get("name").is_none(), "unverified name: {record}");
}

#[test]
fn a_disabled_plugin_and_an_unknown_action_are_refused() {
    let fixture = Fixture::new();
    let output = fixture.run("missing");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(stderr(&output).contains("declares no action \"missing\""));
    assert_eq!(
        fixture.last_record()["error_code"],
        "workflow_action_unknown"
    );

    let disabled = fixture
        .command(&["workflow", "disable", PLUGIN_ID])
        .output()
        .expect("disable");
    assert_eq!(disabled.status.code(), Some(0), "{}", describe(&disabled));
    let output = fixture.run("mark");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output).contains("is disabled; nothing was run"),
        "{}",
        describe(&output)
    );
    assert!(!fixture.marker().exists(), "a disabled plugin ran");
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["error_code"], "workflow_plugin_disabled", "{record}");

    let output = fixture
        .command(&["workflow", "run", "acme.absent", "mark"])
        .output()
        .expect("run an unknown plugin");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(stderr(&output).contains("no workflow plugin with the id acme.absent"));
}

#[test]
fn knobs_are_strict_and_their_defaults_apply_without_a_file() {
    let fixture = Fixture::new();

    // No file: the default ceiling of 600000 ms admits `fixed` and refuses
    // `long`, which declares one millisecond more.
    let output = fixture.run("fixed");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(fixture.marker().exists());
    fs::remove_file(fixture.marker()).expect("reset the marker");
    let output = fixture.run("long");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output)
            .contains("declares timeout_ms 600001, above limits.action_timeout_max_ms 600000"),
        "{}",
        describe(&output)
    );
    assert!(!fixture.marker().exists(), "a refused action ran");
    assert_eq!(
        fixture.last_record()["error_code"],
        "workflow_timeout_refused"
    );

    // A raised ceiling admits it.
    fixture.knobs("[limits]\naction_timeout_max_ms = 700000\n");
    let output = fixture.run("long");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    fs::remove_file(fixture.marker()).expect("the admitted action ran");

    // Unknown keys, later-slice tables and out-of-range values are refused
    // before the store is read, so no run is recorded.
    let recorded = fixture.records().len();
    for (text, needle) in [
        ("[limits]\nkill_grace = 1\n", "unknown key"),
        ("[jobs]\ntimeout_max_ms = 1\n", "unknown key"),
        (
            "[limits]\nkill_grace_ms = 60001\n",
            "limits.kill_grace_ms must be in 0..=60000",
        ),
        (
            "[limits]\naction_timeout_default_ms = 9\naction_timeout_max_ms = 8\n",
            "limits.action_timeout_default_ms must be in 1..=8",
        ),
        (
            "[logs]\nrun_log_files = 0\n",
            "logs.run_log_files must be in 1..=64",
        ),
    ] {
        fixture.knobs(text);
        let output = fixture.run("mark");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{text}: {}",
            describe(&output)
        );
        let message = stderr(&output);
        assert!(
            message.contains("the workflow knobs are invalid"),
            "{text}: {message}"
        );
        assert!(message.contains(needle), "{text}: {message}");
        assert!(!fixture.marker().exists(), "{text}: the plugin ran");
    }
    assert_eq!(
        fixture.records().len(),
        recorded,
        "a refused config logged a run"
    );

    // A knobs file other accounts may read is refused too.
    fixture.knobs("[limits]\n");
    let path = fixture.env.config_home().join("pohunek/workflow.toml");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod knobs");
    let output = fixture.run("mark");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output).contains("mode 600"),
        "{}",
        describe(&output)
    );
}

#[test]
fn an_interrupted_cli_stops_the_plugin_group() {
    let fixture = Fixture::new();
    fixture.knobs("[limits]\nkill_grace_ms = 300\n");
    let cli = fixture
        .command(&["workflow", "run", PLUGIN_ID, "sleepy"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    let child = fixture.pid("child.pid");
    let grandchild = fixture.pid("grandchild.pid");
    let pid = Pid::from_raw(i32::try_from(cli.id()).expect("pid fits"));
    kill(pid, Signal::SIGINT).expect("interrupt the CLI");
    let output = cli.wait_with_output().expect("wait for the CLI");
    assert_eq!(
        output.status.code(),
        Some(SIGINT_EXIT),
        "{}",
        describe(&output)
    );
    assert!(
        stderr(&output).contains("received signal 2"),
        "{}",
        describe(&output)
    );
    wait_gone(child, "the plugin shell");
    wait_gone(grandchild, "the grandchild");
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "interrupted", "{record}");
    assert_eq!(
        record["error_code"], "workflow_action_interrupted",
        "{record}"
    );
}

/// Name of the agent profile of the origin scenario.
const AGENT: &str = "wfagent";

/// A real `pohunekd` with subprocess workers, stopped and reaped on drop.
struct Daemon {
    // Declared before `child`, so leftover workers are reaped first.
    _workers: WorkerGuard,
    child: Child,
}

impl Daemon {
    fn start(env: &TestEnv) -> Self {
        let workers = WorkerGuard::watch(env.root().to_path_buf());
        let log = fs::File::create(env.root().join("daemon.out")).expect("create the daemon log");
        let child = env
            .command(bin_exe("pohunek").with_file_name("pohunekd"))
            .env("SHELL", "/bin/sh")
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("duplicate the log handle"))
            .stderr(log)
            .spawn()
            .expect("spawn pohunekd");
        let socket = env.runtime_dir().join("pohunek/daemon.sock");
        poll_until("the daemon control socket accepts connections", || {
            UnixStream::connect(&socket).ok().map(drop)
        });
        Self {
            _workers: workers,
            child,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

#[test]
fn an_action_inside_a_managed_session_cannot_stop_that_session() {
    let fixture = Fixture::new();
    let env = &fixture.env;
    let cli = bin_exe("pohunek");
    // The agent runs the action once, then keeps the session alive.
    let script = format!(
        "'{}' workflow run {PLUGIN_ID} stop_self -- \"$POHUNEK_SESSION_ID\"; exec /bin/cat",
        cli.display()
    );
    let agents = env.config_home().join("pohunek/agents");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&agents)
        .expect("create the agent profile directory");
    let profile = agents.join(format!("{AGENT}.toml"));
    fs::write(
        &profile,
        format!(
            "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", {}]\n",
            serde_json::to_string(&script).expect("TOML basic string")
        ),
    )
    .expect("write the agent profile");
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).expect("chmod profile");
    let _daemon = Daemon::start(env);

    let cwd = env.cwd().to_str().expect("UTF-8 cwd");
    let created = fixture
        .command(&["session", "new", "--agent", AGENT, "--cwd", cwd, "--json"])
        .output()
        .expect("create the session");
    assert_eq!(created.status.code(), Some(0), "{}", describe(&created));
    let document: Value = serde_json::from_slice(&created.stdout).expect("session JSON");
    let session = document["ok"]["session"]["id"]
        .as_str()
        .or_else(|| document["ok"]["id"].as_str())
        .expect("session id")
        .to_owned();

    let answer = fixture.state_dir().join("stop.json");
    let denied: Value = poll_until("the action to report the stop answer", || {
        if let Some(answer) = fs::read(&answer)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            return Some(answer);
        }
        assert!(
            fixture.session_live(&session),
            "the action stopped its own session: the origin pair did not reach it"
        );
        None
    });
    assert_eq!(
        denied["err"]["code"], "plugin_self_target_denied",
        "{denied}"
    );
    let record = poll_until("the run record", || {
        fixture
            .records()
            .into_iter()
            .find(|record| record["name"] == "stop_self")
    });
    assert_eq!(record["outcome"], "exit_nonzero", "{record}");
    assert!(
        fixture.session_live(&session),
        "the denied stop ended the session"
    );

    // From outside the session the same stop is allowed.
    let stopped = fixture
        .command(&["session", "stop", &session, "--json"])
        .output()
        .expect("stop the session");
    assert_eq!(stopped.status.code(), Some(0), "{}", describe(&stopped));
    poll_until("the session runtime to end", || {
        (!fixture.session_live(&session)).then_some(())
    });
}

/// Fills `pipe` until a write would block, then makes it blocking again, so
/// the next write of the process that inherits it waits for the reader.
/// Returns the number of bytes written.
fn fill(pipe: &std::io::PipeWriter) -> usize {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;

    let descriptor = pipe.as_raw_fd();
    // SAFETY: F_GETFL and F_SETFL only read and change the status flags of a
    // descriptor this test owns.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    assert!(flags >= 0, "read the pipe flags");
    // SAFETY: as above.
    let set = unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(set, 0, "make the pipe non-blocking");
    let mut writer = pipe;
    let mut written = 0;
    // Large writes first, then single bytes for the room a large write
    // cannot use whole.
    for chunk in [vec![b'f'; 4096], vec![b'f'; 1]] {
        loop {
            match writer.write(&chunk) {
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill the pipe: {error}"),
            }
        }
    }
    // SAFETY: as above.
    let reset = unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags) };
    assert_eq!(reset, 0, "make the pipe blocking again");
    written
}

#[test]
fn a_slow_stdout_consumer_still_receives_all_of_the_actions_output() {
    use std::io::Read as _;

    let fixture = Fixture::new();
    let (mut reader, writer) = std::io::pipe().expect("create the stdout pipe");
    // A full pipe: every write the CLI makes to stdout waits for this test.
    let filled = fill(&writer);
    let cli = fixture
        .command(&["workflow", "run", PLUGIN_ID, "burst"])
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    wait_gone(fixture.pid("child.pid"), "the action");
    // timing-allowed: #148 a consumer slower than the runner's idle bound is the subject of this scenario.
    std::thread::sleep(CONSUMER_DELAY);

    let mut stdout = Vec::new();
    reader
        .read_to_end(&mut stdout)
        .expect("read the CLI stdout");
    let output = cli.wait_with_output().expect("wait for the CLI");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        stdout.len(),
        filled + burst_output().len(),
        "{}",
        describe(&output)
    );
    assert!(
        stdout[filled..] == *burst_output().as_bytes(),
        "the action's output arrived changed"
    );
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "ok", "{record}");
    assert_eq!(record["stdout_bytes"], burst_output().len(), "{record}");
}

#[test]
fn a_signal_while_output_waits_for_a_slow_consumer_interrupts_the_run() {
    let fixture = Fixture::new();
    let (reader, writer) = std::io::pipe().expect("create the stdout pipe");
    // A full pipe that is never read: the CLI can only end the drain itself.
    fill(&writer);
    let cli = fixture
        .command(&["workflow", "run", PLUGIN_ID, "burst"])
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    wait_gone(fixture.pid("child.pid"), "the action");
    let pid = Pid::from_raw(i32::try_from(cli.id()).expect("pid fits"));
    kill(pid, Signal::SIGINT).expect("interrupt the CLI");
    let mut cli = cli;
    poll_until("the interrupted CLI to exit", || {
        cli.try_wait().expect("poll the CLI").map(drop)
    });
    let output = cli.wait_with_output().expect("collect the CLI output");
    drop(reader);
    assert_eq!(
        output.status.code(),
        Some(SIGINT_EXIT),
        "{}",
        describe(&output)
    );
    assert!(
        stderr(&output).contains("received signal 2"),
        "{}",
        describe(&output)
    );
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "interrupted", "{record}");
    assert_eq!(
        record["error_code"], "workflow_action_interrupted",
        "{record}"
    );
    assert_eq!(
        record["exit_code"], 0,
        "the action itself exited 0: {record}"
    );
}

/// Kills the escaped process of the `escape` action when the scenario ends.
#[cfg(target_os = "linux")]
struct Escapee(Pid);

#[cfg(target_os = "linux")]
impl Drop for Escapee {
    fn drop(&mut self) {
        let _ = kill(self.0, Signal::SIGKILL);
    }
}

/// Linux only: the scenario needs `/usr/bin/setsid` to leave the group, and
/// macOS ships no `setsid` program at a fixed path.
#[cfg(target_os = "linux")]
#[test]
fn an_escaped_writer_is_cut_after_the_idle_budget_and_reported() {
    let fixture = Fixture::new();
    let mut cli = fixture
        .command(&["workflow", "run", PLUGIN_ID, "escape"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the CLI");
    let _escapee = Escapee(fixture.pid("escapee.pid"));
    // The escaped process writes five times a second for as long as it
    // lives; only a drain that counts that time as idle ends the run.
    poll_until("the CLI to cut the escaped writer", || {
        cli.try_wait().expect("poll the CLI").map(drop)
    });
    let output = cli.wait_with_output().expect("collect the CLI output");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        stderr(&output).contains(
            "the stdout of action escape of workflow plugin acme.work was cut: a process that \
             left the action's process group still holds it open"
        ),
        "{}",
        describe(&output)
    );
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "ok", "{record}");
}
