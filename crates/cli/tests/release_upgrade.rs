//! A session started on the previous release survives `pohunek service
//! upgrade` to this build, driven through the operator path with the real
//! published binaries of the previous release and the real binaries of this
//! build.
//!
//! The previous release installs the service, installs its managed Claude
//! hooks, and starts a session whose agent is a fake `claude` that runs those
//! hook scripts. This build's CLI then checks and upgrades the service. The
//! assertions follow the live session across the daemon swap, through the
//! previous release's own client, and through `stop` and `resume`.
//!
//! The test runs as the invoking user with that user's real default layout
//! (`$HOME/.config`, `$HOME/.local`), which is what a released binary uses, so
//! it must run as a disposable account whose user manager is dedicated to it.
//! `scripts/upgrade-test` creates that account, runs this test in it, and
//! removes it; do not run the test directly on a machine with a real
//! installation. It is ignored unless `POHUNEK_SYSTEMD_E2E=1` opts in.
//!
//! Inputs, all required, none defaulted:
//! - `POHUNEK_PREVIOUS_BIN_DIR`: directory with the previous release's
//!   `pohunek`, `pohunekd`, and `pohunek-sessiond`;
//! - `POHUNEK_HEAD_BIN_DIR`: the same three binaries of this build, with a
//!   version that differs from the previous release;
//! - `POHUNEK_UPGRADE_ARTIFACTS_DIR`: writable directory receiving the
//!   previous release's store, journals, `service.toml`, and `--json` outputs.
//!
//! Results of both CLIs are read as untyped JSON and only the fields named
//! below are consulted, so a schema difference between the releases cannot
//! fail the test before the behavior it exercises does.

#![cfg(target_os = "linux")]

// Rust guideline compliant 2026-10-05

use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_test_support::wait::poll_until;
use serde_json::Value;

/// Source of the fake agent the session launches.
const FAKE_AGENT: &str = include_str!("support/release_upgrade_agent.sh");

/// Shell the fake agent is a copy of.
const SHELL: &str = "/bin/sh";

/// Host profile name of the fake agent.
const PROFILE: &str = "fake-claude";

/// Native id the fresh launch reports through the previous release's hook.
/// The worker keeps the first launch identity, so this stays the session's
/// native reference, and `resume` relaunches with it.
const NATIVE_BEFORE: &str = "native-before-upgrade";

/// Id reported after the upgrade. A later report does not replace the launch
/// identity; it becomes the session's active agent session id.
const NATIVE_AFTER: &str = "native-after-upgrade";

/// Hook event ids of the notifications the test waits for; each carries a
/// distinct one so the test can tell which phase delivered it. The fake agent
/// also sends one at its first launch (`evt-before-upgrade`); the previous
/// release's own notification hook is not accepted by its own daemon, so that
/// one is not observable and not asserted.
const EVENT_AFTER_UPGRADE: &str = "evt-after-upgrade";
const EVENT_AFTER_RESUME: &str = "evt-after-resume";

/// Environment a child CLI receives; nothing else is inherited, so no
/// `POHUNEK_*` input of this test reaches the daemon or the CLIs.
const CHILD_ENV: [&str; 4] = [
    "PATH",
    "HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Names a state subdirectory the golden capture never copies: the host
/// identity and approval key live there, and the log directory is not state a
/// fixture needs.
const GOLDEN_SKIPPED_DIRS: [&str; 2] = ["host", "logs"];

/// File name fragments the golden capture never copies.
const GOLDEN_SKIPPED_NAMES: [&str; 3] = ["key", "secret", "token"];

/// Largest file the golden capture copies. The worker journal is bounded at
/// 1 MiB, so anything larger is not a journal.
const GOLDEN_MAX_BYTES: u64 = 1024 * 1024;

/// Sequence number of the next recorded CLI call.
static CALLS: AtomicU64 = AtomicU64::new(0);

/// One CLI binary and the label its failures carry.
struct Cli {
    label: &'static str,
    bin: PathBuf,
    /// Directory receiving one text file per call: arguments, status, stdout
    /// and stderr, so a failed run leaves the last outputs behind.
    calls: PathBuf,
}

impl Cli {
    fn new(label: &'static str, dir: &Path, artifacts: &Path) -> Self {
        let bin = dir.join("pohunek");
        assert!(bin.is_file(), "{label}: {} is missing", bin.display());
        Self {
            label,
            bin,
            calls: artifacts.join("calls"),
        }
    }

    /// Writes one call record; a failure to record is reported, never fatal.
    fn record(&self, args: &[&str], output: &Output) {
        let number = CALLS.fetch_add(1, Ordering::Relaxed);
        let path = self.calls.join(format!("{number:04}-{}.txt", self.label));
        let text = format!(
            "pohunek {}\nstatus: {}\n--- stdout\n{}\n--- stderr\n{}\n",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        if let Err(error) = fs::create_dir_all(&self.calls).and_then(|()| fs::write(&path, text)) {
            eprintln!("cannot record {}: {error}", path.display());
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.bin);
        command.env_clear().args(args);
        for name in CHILD_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let output = command.output().unwrap_or_else(|error| {
            panic!("{}: cannot run {}: {error}", self.label, self.bin.display())
        });
        self.record(args, &output);
        output
    }

    /// Runs `args` and returns the stdout JSON, panicking with both streams
    /// when the command fails or prints something else.
    fn json(&self, args: &[&str]) -> Value {
        self.try_json(args).unwrap_or_else(|failure| {
            panic!(
                "{}: `pohunek {}` failed: {failure}",
                self.label,
                args.join(" ")
            )
        })
    }

    /// Returns the `ok` payload of the `--json` envelope, or an error naming
    /// the typed `err` payload, the exit status, and both streams.
    fn try_json(&self, args: &[&str]) -> Result<Value, String> {
        let output = self.run(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_envelope(&stdout, output.status.success()).map_err(|reason| {
            format!(
                "{reason}\nstatus: {}\nstdout: {stdout}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn version(&self) -> String {
        let output = self.run(&["--version"]);
        assert!(output.status.success(), "{}: --version failed", self.label);
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("{}: --version has no version", self.label))
            .to_owned()
    }
}

/// Extracts the `ok` payload from a `--json` document of either release:
/// `{"cli_version", "protocol", "ok": ...}` on success and
/// `{"cli_version", "protocol", "err": {"class", "code", "msg"}}` on failure.
fn parse_envelope(stdout: &str, success: bool) -> Result<Value, String> {
    let mut document: Value =
        serde_json::from_str(stdout).map_err(|error| format!("stdout is not JSON: {error}"))?;
    if let Some(err) = document.get("err") {
        let field = |name: &str| err.get(name).and_then(Value::as_str).unwrap_or("?");
        return Err(format!(
            "typed error {}/{}: {}",
            field("class"),
            field("code"),
            field("msg")
        ));
    }
    match document.get_mut("ok") {
        Some(ok) if success => Ok(ok.take()),
        Some(_) => Err("a failing exit status carried an `ok` payload".to_owned()),
        None => Err("the document has neither `ok` nor `err`".to_owned()),
    }
}

/// Checks a `service upgrade --json` payload. v0.33.1's report has no
/// `accepted_runtime_loss` (nor `preflight`); an absent field means nothing
/// was accepted, while an explicit `true` is refused.
fn assert_upgrade_report(report: &Value, from: &str, to: &str) {
    assert_eq!(
        report["unchanged"], false,
        "the daemon was not swapped: {report}"
    );
    assert_eq!(report["from_version"], from, "{report}");
    assert_eq!(report["to_version"], to, "{report}");
    assert_ne!(
        report.get("accepted_runtime_loss"),
        Some(&Value::Bool(true)),
        "the upgrade accepted runtime loss: {report}"
    );
}

/// Whether a failure from [`Cli::try_json`] is exactly the typed error
/// `agent_profile_changed`.
fn is_profile_change(failure: &str) -> bool {
    failure.starts_with("typed error ") && failure.contains("/agent_profile_changed:")
}

/// Inputs and paths of one run.
struct Run {
    home: PathBuf,
    previous_dir: PathBuf,
    head_dir: PathBuf,
    artifacts: PathBuf,
}

impl Run {
    fn from_env() -> Self {
        assert_eq!(
            std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
            Ok("1"),
            "set POHUNEK_SYSTEMD_E2E=1 explicitly, and run through scripts/upgrade-test"
        );
        for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            assert!(
                std::env::var_os(name).is_some(),
                "{name} must name the user manager of the account running this test"
            );
        }
        let path = |name: &str| {
            let value = std::env::var_os(name).unwrap_or_else(|| panic!("{name} is required"));
            let path = PathBuf::from(value);
            assert!(path.is_absolute(), "{name} must be an absolute path");
            path
        };
        Self {
            home: path("HOME"),
            previous_dir: path("POHUNEK_PREVIOUS_BIN_DIR"),
            head_dir: path("POHUNEK_HEAD_BIN_DIR"),
            artifacts: path("POHUNEK_UPGRADE_ARTIFACTS_DIR"),
        }
    }

    fn agent_dir(&self) -> PathBuf {
        self.home.join("fake-agent")
    }

    fn agent_log(&self) -> PathBuf {
        self.agent_dir().join("launches.log")
    }

    fn claude_hooks(&self) -> PathBuf {
        self.home.join(".claude/hooks")
    }

    /// Where the previous release's daemon keeps `service.toml` and the host
    /// profiles under the default XDG layout.
    fn config_dir(&self) -> PathBuf {
        self.home.join(".config/pohunek")
    }

    fn data_dir(&self) -> PathBuf {
        self.home.join(".local/share/pohunek")
    }

    fn state_dir(&self) -> PathBuf {
        self.home.join(".local/state/pohunek")
    }

    /// Waits until the fake agent has logged `count` launches and returns them.
    /// A runtime is live before its agent appends its entry, so a launch count
    /// is read only through this wait.
    fn wait_launches(&self, count: usize) -> Vec<String> {
        poll_until(&format!("{count} agent launches logged"), || {
            let launches = self.launches();
            (launches.len() >= count).then_some(launches)
        })
    }

    /// Launch lines the fake agent appended, oldest first.
    fn launches(&self) -> Vec<String> {
        fs::read_to_string(self.agent_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Runtime identity of a session as `session inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Runtime {
    state: String,
    worker_instance_id: String,
    generation: u64,
}

/// Reads the runtime identity of a `session inspect` payload. The previous
/// release (v0.33.0, protocol 3) spells the worker instance `runtime_id`; v0.33.1
/// and this build spell it `worker_instance_id`. While protocol 3 is in the
/// CLI window, this build includes both names for an older Hermes plugin;
/// their values must agree.
fn runtime(info: &Value) -> Option<Runtime> {
    let runtime = info.get("runtime")?;
    let spelled = |name: &str| runtime.get(name).and_then(Value::as_str);
    let worker_instance_id = match (spelled("worker_instance_id"), spelled("runtime_id")) {
        (Some(worker), Some(legacy)) if worker == legacy => worker.to_owned(),
        (Some(_), Some(_)) => {
            panic!("runtime carries conflicting worker_instance_id and runtime_id: {runtime}")
        }
        (Some(id), None) | (None, Some(id)) => id.to_owned(),
        (None, None) => return None,
    };
    Some(Runtime {
        state: runtime.get("state")?.as_str()?.to_owned(),
        worker_instance_id,
        generation: match runtime.get("runtime_generation")? {
            Value::String(text) => text.parse().ok()?,
            Value::Number(number) => number.as_u64()?,
            _ => return None,
        },
    })
}

fn inspect(cli: &Cli, id: &str) -> Option<Value> {
    cli.try_json(&["session", "inspect", id, "--json"]).ok()
}

/// Waits until `id` reports a live runtime and returns its identity.
fn wait_live(cli: &Cli, id: &str) -> Runtime {
    poll_until(&format!("{id} live through {}", cli.label), || {
        runtime(&inspect(cli, id)?).filter(|runtime| runtime.state == "live")
    })
}

fn screen(cli: &Cli, id: &str) -> Option<String> {
    let value = cli.try_json(&["session", "screen", id, "--json"]).ok()?;
    let lines = value.get("visible_lines")?.as_array()?;
    Some(
        lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn wait_screen(cli: &Cli, id: &str, needle: &str) -> String {
    poll_until(
        &format!("`{needle}` on the screen of {id} via {}", cli.label),
        || screen(cli, id).filter(|text| text.contains(needle)),
    )
}

fn native_id(cli: &Cli, id: &str) -> Option<String> {
    inspect(cli, id)?
        .get("native_session_id")?
        .as_str()
        .map(str::to_owned)
}

fn active_agent_id(cli: &Cli, id: &str) -> Option<String> {
    inspect(cli, id)?
        .get("active_agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

fn wait_active_agent_id(cli: &Cli, id: &str, expected: &str) {
    poll_until(
        &format!(
            "active agent session id {expected} of {id} via {}",
            cli.label
        ),
        || (active_agent_id(cli, id).as_deref() == Some(expected)).then_some(()),
    );
}

fn wait_native_id(cli: &Cli, id: &str, expected: &str) {
    poll_until(
        &format!("native id {expected} of {id} via {}", cli.label),
        || (native_id(cli, id).as_deref() == Some(expected)).then_some(()),
    );
}

/// Waits until a notification of the session mentions the hook event id.
fn wait_notification(cli: &Cli, id: &str, event: &str) {
    poll_until(
        &format!("notification {event} of {id} via {}", cli.label),
        || {
            let list = cli
                .try_json(&["notifications", "list", "--session", id, "--json"])
                .ok()?;
            list.get("notifications")?
                .as_array()?
                .iter()
                .any(|record| record.to_string().contains(event))
                .then_some(())
        },
    );
}

fn input(cli: &Cli, id: &str, text: &str) {
    let result = cli.json(&["session", "input", id, text, "--json"]);
    assert_eq!(
        result.get("accepted").and_then(Value::as_bool),
        Some(true),
        "{}: input was not accepted: {result}",
        cli.label
    );
}

fn service_status(cli: &Cli) -> Value {
    cli.json(&["service", "status", "--json"])
}

fn daemon_pid(status: &Value) -> u64 {
    status
        .pointer("/daemon/pid")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("the daemon job has no pid: {status}"))
}

/// Version directory the session's worker job runs from, when it has one.
fn worker_version(status: &Value, id: &str) -> Option<String> {
    status
        .get("workers")?
        .as_array()?
        .iter()
        .find(|worker| worker.get("session_id").and_then(Value::as_str) == Some(id))?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

/// Writes the fake agent, its host profile, and the Claude config directory
/// the managed hooks are installed into.
fn prepare_agent(run: &Run) {
    let private = |path: &Path| {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    };
    private(&run.agent_dir());
    private(&run.home.join(".claude"));
    // The worker designates the agent process by an executable name containing
    // the provider name, and a script's executable is its interpreter. The
    // agent is therefore a copy of the shell named `fake-claude-sh` running the
    // script, which is what the profile launches.
    let script = run.agent_dir().join("fake-claude.sh");
    fs::write(&script, FAKE_AGENT).expect("write the fake agent");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod the fake agent");
    let agent = run.agent_dir().join("fake-claude-sh");
    let shell = fs::canonicalize(SHELL).expect("resolve the shell");
    fs::copy(&shell, &agent).expect("copy the shell as the agent executable");
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).expect("chmod the agent");

    let agents = run.config_dir().join("agents");
    private(&agents);
    // Input is pasted as plain text: the fake agent reads lines with `read`
    // and would see the bracketed-paste escapes otherwise.
    let profile = format!(
        "base = \"claude\"\nprogram = \"{}\"\nargs = [\"{}\"]\n[env]\nFAKE_AGENT_LOG = \"{}\"\n\
         FAKE_AGENT_HOOKS = \"{}\"\nFAKE_AGENT_NATIVE_ID = \"{NATIVE_BEFORE}\"\n\
         [input_rules]\nbracketed_paste = false\n",
        agent.display(),
        script.display(),
        run.agent_log().display(),
        run.claude_hooks().display(),
    );
    let path = agents.join(format!("{PROFILE}.toml"));
    fs::write(&path, profile).expect("write the host profile");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod the profile");
}

/// Copies the previous release's store, worker journals, `service.toml`, and
/// the given `--json` results into the artifacts directory.
fn capture_goldens(run: &Run, version: &str, outputs: &[(&str, &Value)]) {
    let root = run.artifacts.join(format!("previous-{version}"));
    let copy = |from: &Path, to: &Path| {
        fs::create_dir_all(to.parent().expect("golden path has a parent"))
            .expect("create the golden directory");
        fs::copy(from, to).unwrap_or_else(|error| panic!("copy {}: {error}", from.display()));
    };
    copy(
        &run.data_dir().join("metadata.jsonl"),
        &root.join("store/metadata.jsonl"),
    );
    copy(
        &run.config_dir().join("service.toml"),
        &root.join("service.toml"),
    );
    copy_state(&run.state_dir(), &run.state_dir(), &root.join("state"));
    for (name, value) in outputs {
        let path = root.join("protocol").join(format!("{name}.json"));
        fs::create_dir_all(path.parent().expect("golden path has a parent"))
            .expect("create the golden directory");
        fs::write(
            &path,
            serde_json::to_string_pretty(value).expect("render JSON"),
        )
        .expect("write a protocol golden");
    }
}

/// Copies the regular files of `dir` below `base` into `target`, keeping
/// relative paths and skipping what [`GOLDEN_SKIPPED_DIRS`] and
/// [`GOLDEN_SKIPPED_NAMES`] exclude.
fn copy_state(base: &Path, dir: &Path, target: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.map(|entry| entry.expect("read the state directory")) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type().expect("inspect a state entry");
        let path = entry.path();
        if kind.is_dir() {
            if !GOLDEN_SKIPPED_DIRS.contains(&name.as_str()) {
                copy_state(base, &path, target);
            }
        } else if kind.is_file()
            && !GOLDEN_SKIPPED_NAMES.iter().any(|part| name.contains(part))
            && entry
                .metadata()
                .is_ok_and(|meta| meta.len() <= GOLDEN_MAX_BYTES)
        {
            let relative = path.strip_prefix(base).expect("entry is below the base");
            let to = target.join(relative);
            fs::create_dir_all(to.parent().expect("golden path has a parent"))
                .expect("create the golden directory");
            fs::copy(&path, &to).expect("copy a state file");
        }
    }
}

/// Both releases, the versions they report, and the session under test.
struct Upgrade {
    run: Run,
    previous: Cli,
    head: Cli,
    previous_version: String,
    head_version: String,
    id: String,
}

/// The previous release installs itself and its managed hooks, then runs a
/// session whose agent reports a native id and a notification through them.
fn start_on_previous(run: Run, previous: Cli, head: Cli) -> (Upgrade, Runtime, u64) {
    let (previous_version, head_version) = (previous.version(), head.version());
    assert_ne!(
        previous_version, head_version,
        "an upgrade to the same version is a no-op and proves nothing"
    );
    eprintln!("upgrade {previous_version} -> {head_version}");
    prepare_agent(&run);
    let from = run.previous_dir.display().to_string();
    previous.json(&["service", "install", "--from", &from, "--json"]);
    previous.json(&["integration", "install", "--agent", "claude", "--json"]);
    for hook in ["pohunek-agent-state.sh", "pohunek-agent-notify.sh"] {
        let installed = run.claude_hooks().join(hook);
        assert!(
            installed.is_file(),
            "{} was not installed",
            installed.display()
        );
    }
    let cwd = run.home.display().to_string();
    let created = previous.json(&[
        "session",
        "new",
        "--agent",
        PROFILE,
        "--name",
        "upgrade-fake",
        "--cwd",
        &cwd,
        "--json",
    ]);
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("session new returned no id: {created}"))
        .to_owned();
    let before = wait_live(&previous, &id);
    wait_screen(&previous, &id, "fake-agent ready");
    wait_native_id(&previous, &id, NATIVE_BEFORE);
    let launches = run.wait_launches(1);
    assert_eq!(
        launches.len(),
        1,
        "one launch before the upgrade: {launches:?}"
    );
    let daemon = daemon_pid(&service_status(&previous));
    let upgrade = Upgrade {
        run,
        previous,
        head,
        previous_version,
        head_version,
        id,
    };
    (upgrade, before, daemon)
}

impl Upgrade {
    /// Stores the previous release's store and protocol goldens, taken before
    /// this build migrates anything in place.
    fn capture_previous_goldens(&self) {
        let (previous, id) = (&self.previous, &self.id);
        let list = previous.json(&["session", "list", "--json"]);
        let info = previous.json(&["session", "inspect", id, "--json"]);
        let shown = previous.json(&["session", "screen", id, "--json"]);
        let notes = previous.json(&["notifications", "list", "--session", id, "--json"]);
        let health = previous.json(&["health", "--json"]);
        let status = service_status(previous);
        capture_goldens(
            &self.run,
            &self.previous_version,
            &[
                ("session-list", &list),
                ("session-inspect", &info),
                ("session-screen", &shown),
                ("notifications-list", &notes),
                ("health", &health),
                ("service-status", &status),
            ],
        );
    }

    /// The operator path: this build's CLI checks, then upgrades. Neither
    /// accepts runtime loss, so a refusal by the adoption preflight fails here.
    fn upgrade(&self) {
        let check = self.head.json(&["service", "check", "--json"]);
        eprintln!("service check: {check}");
        let from = self.run.head_dir.display().to_string();
        let report = self
            .head
            .json(&["service", "upgrade", "--from", &from, "--json"]);
        eprintln!("service upgrade: {report}");
        assert_upgrade_report(&report, &self.previous_version, &self.head_version);
    }

    /// The same worker still owns the session: adoption, not a relaunch.
    fn assert_adopted(&self, before: &Runtime, daemon_before: u64) -> Runtime {
        let after = wait_live(&self.head, &self.id);
        assert_eq!(&after, before, "the upgrade must adopt the live worker");
        let status = service_status(&self.head);
        assert_eq!(status["active_version"], self.head_version.as_str());
        assert_ne!(
            daemon_pid(&status),
            daemon_before,
            "the daemon was not restarted"
        );
        assert_eq!(
            worker_version(&status, &self.id).as_deref(),
            Some(self.previous_version.as_str()),
            "the adopted worker keeps running the previous release: {status}"
        );
        after
    }

    /// `screen` and `input` reach the adopted worker through the new daemon,
    /// and the previous release's hooks, running in the previous release's
    /// worker, still reach the new daemon.
    fn assert_session_works(&self) {
        let (head, id) = (&self.head, &self.id);
        wait_screen(head, id, "fake-agent ready");
        input(head, id, "echo:input-after-upgrade");
        wait_screen(head, id, "fake-agent ack input-after-upgrade");
        wait_native_id(head, id, NATIVE_BEFORE);
        input(head, id, &format!("report:{NATIVE_AFTER}"));
        wait_active_agent_id(head, id, NATIVE_AFTER);
        // The runbook step after an upgrade: reinstall the managed hooks. The
        // previous release's notification hook is dropped by every daemon that
        // expects the current hook, so the new one is installed over it while
        // the session keeps running in the adopted worker.
        head.json(&["integration", "install", "--agent", "claude", "--json"]);
        input(head, id, &format!("stopfail:{EVENT_AFTER_UPGRADE}"));
        wait_notification(head, id, EVENT_AFTER_UPGRADE);
    }

    /// A client of the previous release talks to the new daemon: reads and a
    /// write, within the protocol window.
    fn assert_previous_client(&self, after: &Runtime) {
        let (previous, id) = (&self.previous, &self.id);
        let listed = previous.json(&["session", "list", "--json"]);
        assert!(
            listed.to_string().contains(id),
            "previous client lost {id}: {listed}"
        );
        assert_eq!(
            runtime(&previous.json(&["session", "inspect", id, "--json"])).as_ref(),
            Some(after)
        );
        previous.json(&["health", "--json"]);
        wait_notification(previous, id, EVENT_AFTER_UPGRADE);
        input(previous, id, "echo:input-from-previous-client");
        wait_screen(previous, id, "fake-agent ack input-from-previous-client");
    }

    /// Stops the session and waits until its runtime has ended.
    fn stop_and_wait(&self) {
        let (head, id) = (&self.head, &self.id);
        let stopped = head.json(&["session", "stop", id, "--json"]);
        assert_eq!(
            stopped["stopped"], true,
            "stop did not stop the session: {stopped}"
        );
        poll_until(&format!("{id} runtime ended"), || {
            runtime(&inspect(head, id)?)
                .filter(|runtime| matches!(runtime.state.as_str(), "terminal" | "lost"))
        });
    }

    /// `stop` ends the adopted runtime, then `resume` relaunches with the native
    /// id kept across the upgrade, in a new worker from this build. A session
    /// launched from a host profile by a release that does not freeze the
    /// profile revision is refused by a plain `resume` with the typed error
    /// `agent_profile_changed` and then resumed with `--accept-profile-change`;
    /// a release that freezes it resumes plainly. Any other failure fails the
    /// test. Either way the relaunch freezes the profile, so a later plain
    /// `resume` works.
    fn assert_stop_and_resume(&self, after: &Runtime) {
        let (head, id) = (&self.head, &self.id);
        self.stop_and_wait();
        match head.try_json(&["session", "resume", id, "--json"]) {
            Ok(_) => {}
            Err(refused) => {
                assert!(
                    is_profile_change(&refused),
                    "plain resume failed with something other than agent_profile_changed: {refused}"
                );
                // The refusal is synchronous, so no launch can still be pending.
                assert_eq!(
                    self.run.launches().len(),
                    1,
                    "the refused resume launched nothing"
                );
                head.json(&["session", "resume", id, "--accept-profile-change", "--json"]);
            }
        }
        let resumed = wait_live(head, id);
        assert!(
            resumed.generation > after.generation
                && resumed.worker_instance_id != after.worker_instance_id,
            "resume must start a new runtime generation: {after:?} -> {resumed:?}"
        );
        wait_screen(head, id, &format!("args=[--resume {NATIVE_BEFORE}]"));
        let launches = self.run.wait_launches(2);
        assert_eq!(launches.len(), 2, "one launch per runtime: {launches:?}");
        assert!(
            launches[1].contains(&format!("--resume {NATIVE_BEFORE}")),
            "resume launched without the native reference: {launches:?}"
        );
        let status = poll_until("the resumed worker runs from this build", || {
            let status = service_status(head);
            (worker_version(&status, id).as_deref() == Some(self.head_version.as_str()))
                .then_some(status)
        });
        assert_eq!(status["active_version"], self.head_version.as_str());
        wait_native_id(head, id, NATIVE_BEFORE);
        wait_notification(head, id, EVENT_AFTER_RESUME);

        // The relaunch froze the profile revision.
        self.stop_and_wait();
        head.json(&["session", "resume", id, "--json"]);
        let again = wait_live(head, id);
        assert!(
            again.generation > resumed.generation,
            "the plain resume must start a new generation: {resumed:?} -> {again:?}"
        );
        let launches = self.run.wait_launches(3);
        assert_eq!(launches.len(), 3, "one launch per runtime: {launches:?}");
    }
}

#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1, a disposable account with a systemd user manager, and the release binaries; run scripts/upgrade-test"]
fn previous_release_session_survives_the_upgrade_to_head() {
    let run = Run::from_env();
    let previous = Cli::new("previous", &run.previous_dir, &run.artifacts);
    let head = Cli::new("head", &run.head_dir, &run.artifacts);
    let (upgrade, before, daemon_before) = start_on_previous(run, previous, head);
    upgrade.capture_previous_goldens();
    upgrade.upgrade();
    let after = upgrade.assert_adopted(&before, daemon_before);
    upgrade.assert_session_works();
    upgrade.assert_previous_client(&after);
    upgrade.assert_stop_and_resume(&after);
    upgrade.head.json(&[
        "service",
        "uninstall",
        "--stop-sessions",
        "--purge",
        "--json",
    ]);
}

/// Real `--json` documents captured from the v0.33.1 and this build's CLIs
/// (`tests/fixtures/release_upgrade`) and the recorded v0.33.0 protocol 3
/// results.
#[cfg(test)]
mod tests {
    use super::*;

    const V0331_INSPECT: &str =
        include_str!("fixtures/release_upgrade/v0.33.1-session-inspect.json");
    const V0331_SCREEN: &str = include_str!("fixtures/release_upgrade/v0.33.1-session-screen.json");
    const V0331_ERROR: &str = include_str!("fixtures/release_upgrade/v0.33.1-error.json");
    const HEAD_INSPECT: &str = include_str!("fixtures/release_upgrade/head-session-inspect.json");
    const HEAD_NOTIFICATIONS: &str =
        include_str!("fixtures/release_upgrade/head-notifications-list.json");
    const HEAD_ERROR: &str = include_str!("fixtures/release_upgrade/head-error.json");
    const V3_RESULTS: &str = include_str!("../../protocol/tests/fixtures/compat/v3/results.json");

    /// The recorded v0.33.0 result of `method`.
    fn v3_result(method: &str) -> Value {
        let recording: Value = serde_json::from_str(V3_RESULTS).expect("parse the v3 recording");
        assert_eq!(recording["release"], "v0.33.0");
        recording["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|entry| entry["method"] == method)
            .unwrap_or_else(|| panic!("no recorded {method}"))["result"]
            .clone()
    }

    #[test]
    fn envelopes_of_both_releases_yield_their_payload() {
        for document in [V0331_INSPECT, HEAD_INSPECT] {
            let info = parse_envelope(document, true).expect("ok envelope");
            assert!(info.get("cli_version").is_none(), "the envelope is removed");
            let runtime = runtime(&info).expect("runtime identity");
            assert_eq!(runtime.state, "live");
            assert!(runtime.generation >= 1);
            assert!(runtime.worker_instance_id.starts_with("runtime-"));
            assert_eq!(info["native_session_id"], "native-before-upgrade");
        }
        let screen = parse_envelope(V0331_SCREEN, true).expect("screen envelope");
        assert!(screen["visible_lines"]
            .as_array()
            .is_some_and(|lines| !lines.is_empty()));
        let list = parse_envelope(HEAD_NOTIFICATIONS, true).expect("notification envelope");
        assert!(list["notifications"]
            .as_array()
            .is_some_and(|records| records.iter().any(|r| r.to_string().contains("evt-x"))));
    }

    #[test]
    fn an_error_envelope_is_a_typed_failure() {
        for document in [V0331_ERROR, HEAD_ERROR] {
            let failure = parse_envelope(document, false).expect_err("error envelope");
            assert!(failure.contains("runtime/session_not_found"), "{failure}");
            // A typed error is a failure whatever the exit status says.
            let _ = parse_envelope(document, true).unwrap_err();
        }
    }

    /// `service upgrade --json` of v0.33.1: the fields of its `UpgradeReport`
    /// (`git show v0.33.1:crates/cli/src/service/report.rs`), no preflight.
    const V0331_UPGRADE: &str = r#"{"cli_version":"0.33.1","protocol":{"minimum":4,"maximum":4},
        "ok":{"from_version":"0.33.0","to_version":"0.33.1","unchanged":false,"resumed":false,
        "rolled_back":null,"removed_versions":[],"kept_versions":[],"gc_error":null}}"#;

    #[test]
    fn the_v0_33_1_upgrade_report_is_accepted_without_the_newer_fields() {
        let report = parse_envelope(V0331_UPGRADE, true).expect("upgrade envelope");
        assert_upgrade_report(&report, "0.33.0", "0.33.1");
    }

    #[test]
    #[should_panic(expected = "accepted runtime loss")]
    fn an_explicit_runtime_loss_acceptance_is_refused() {
        let mut report = parse_envelope(V0331_UPGRADE, true).expect("upgrade envelope");
        report["accepted_runtime_loss"] = Value::Bool(true);
        assert_upgrade_report(&report, "0.33.0", "0.33.1");
    }

    #[test]
    #[should_panic(expected = "was not swapped")]
    fn an_unchanged_upgrade_is_refused() {
        let mut report = parse_envelope(V0331_UPGRADE, true).expect("upgrade envelope");
        report["unchanged"] = Value::Bool(true);
        assert_upgrade_report(&report, "0.33.0", "0.33.1");
    }

    #[test]
    fn only_the_typed_profile_change_error_is_recognized() {
        let changed = r#"{"cli_version":"0.33.1","protocol":{"minimum":4,"maximum":4},
            "err":{"class":"runtime","code":"agent_profile_changed","msg":"no recorded revision"}}"#;
        let other = changed.replace("agent_profile_changed", "session_not_found");
        let failure = parse_envelope(changed, false).expect_err("typed error");
        assert!(is_profile_change(&failure), "{failure}");
        let failure = parse_envelope(&other, false).expect_err("typed error");
        assert!(!is_profile_change(&failure), "{failure}");
        assert!(!is_profile_change(
            "exit status: 1\nstdout: agent_profile_changed"
        ));
    }

    #[test]
    fn malformed_documents_are_failures() {
        let _ = parse_envelope("not json", true).unwrap_err();
        let _ = parse_envelope("{\"cli_version\":\"1\"}", true).unwrap_err();
        let _ = parse_envelope(V0331_INSPECT, false).unwrap_err();
    }

    #[test]
    fn the_v0_33_0_spelling_of_the_runtime_is_normalized() {
        let recorded = v3_result("session.inspect");
        assert!(recorded["runtime"].get("runtime_id").is_some());
        assert!(recorded["runtime"].get("worker_instance_id").is_none());
        let runtime = runtime(&recorded).expect("v0.33.0 runtime");
        assert_eq!(runtime.worker_instance_id, "runtime-42");
        assert_eq!(runtime.generation, 3);
    }

    #[test]
    fn matching_spellings_at_once_are_normalized() {
        let info = serde_json::json!({"runtime": {
            "state": "live", "runtime_generation": "1",
            "worker_instance_id": "a", "runtime_id": "a"}});
        assert_eq!(
            runtime(&info)
                .expect("matching identity")
                .worker_instance_id,
            "a"
        );
    }

    #[test]
    #[should_panic(expected = "conflicting worker_instance_id and runtime_id")]
    fn conflicting_spellings_at_once_are_refused() {
        let info = serde_json::json!({"runtime": {
            "state": "live", "runtime_generation": "1",
            "worker_instance_id": "a", "runtime_id": "b"}});
        let _ = runtime(&info);
    }

    /// The other payload fields the assertions read keep their spelling in the
    /// v0.33.0 recordings.
    #[test]
    fn the_other_fields_have_one_spelling_in_both_releases() {
        let inspect = v3_result("session.inspect");
        assert!(inspect.get("native_session_id").is_some());
        assert!(inspect.get("active_agent_session_id").is_some());
        assert!(v3_result("session.screen")["visible_lines"].is_array());
        assert!(v3_result("session.input")["accepted"].is_boolean());
        assert!(v3_result("session.stop")["stopped"].is_boolean());
        assert!(v3_result("notification.list")["notifications"].is_array());
        assert!(v3_result("daemon.health").get("status").is_some());
    }
}
