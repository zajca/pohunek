//! A session started on the previous release survives `pohunek service
//! upgrade` to this build, driven through the operator path with the real
//! published binaries of the previous release and the real binaries of this
//! build.
//!
//! The previous release starts the host in one of two states, which the test
//! reads from that release's own `plugin profile list` after its `service
//! install`. On the built-in Claude and Codex runtimes (no package installed, no
//! profile pinned) the upgrade installs the head's packages and pins every host
//! profile to them. On its own official packages (the previous release's
//! `service install` installed and pinned them) the upgrade installs the head's
//! packages beside them. A head whose package is a different digest of a newer
//! version repins the profiles to the head digest (`repin`, applied `repinned`,
//! naming the previous digest and version). The same digest keeps the pins, and
//! the same version with another digest is refused by the registry. A head at
//! `RUNTIME_RANGE_RELEASE` or later must show the newer-version outcome for a
//! package that widens the previous one: the previous release predates
//! `RUNTIME_RANGE_RELEASE`, or the head ships another digest whose range accepts
//! the previous range's `below` bound. A head that ships the previous release's
//! own digest must keep the pins and adopt nothing onto a newer package.
//!
//! Either way the test installs managed Claude hooks, starts a stopped and a
//! live session of the Claude profiles, each launched by a fake `claude` that
//! runs the hook scripts the release wrote, and runs one `pohunek service
//! upgrade` of this build without `--accept-runtime-loss`. Before the upgrade,
//! a package start rewrites the fake `claude` to answer the previous package's
//! exclusive `below` bound when the head's package accepts it. The previous
//! package then refuses the agent the stopped session would resume with, so
//! only the adoption onto the head package resumes it. The preflight judges the
//! stopped session `adoptable_by_newer_package` and the live session
//! `adoptable`. The assertions follow the live session across the daemon swap,
//! through the previous release's own client, and the stopped session through a
//! plain `resume` onto the package digest its profile names. New sessions from
//! the Claude and Codex profiles then launch through the packages. The head's
//! downgrade guard then refuses an older archive through `plugin update`.
//!
//! The fake agents are scripts whose interpreter is a copy of the shell called
//! `fake-claude-sh` or `fake-codex-sh`. The worker designates the agent process
//! by an executable name containing the provider name, and a script's
//! executable is its interpreter, so the copy's name is what qualifies the
//! process. The profiles launch the scripts directly, so the package's version
//! probe runs them too, and they answer `--version` with the lowest release
//! the head package's descriptor accepts.
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
//!   version that differs from the previous release, the release layout beside
//!   them (`runtime-catalog-anchor.json` and `runtime/` with `runtime-catalog.json`
//!   and `packages/claude.tar.zst` and `packages/codex.tar.zst`), which is what
//!   `scripts/upgrade-test` stages;
//! - `POHUNEK_UPGRADE_ARTIFACTS_DIR`: writable directory receiving the
//!   previous release's store, journals, `service.toml`, and `--json` outputs.
//!
//! One input is optional:
//! - `POHUNEK_UPGRADE_DOWNGRADE_DIR`: the directory `pohunek release
//!   test-runtime --downgrade` wrote beside the head's runtime tree, with a
//!   `runtime-catalog.json` and `packages/claude.tar.zst` at a lower version
//!   under the head's signing key and sequence. It is absent when the head's
//!   xtask predates `--downgrade`, which is why it is not required. When it is
//!   set, the test runs the downgrade guard. For a head at
//!   `RUNTIME_RANGE_RELEASE` or later it must be set, and its absence fails.
//!
//! The head's archives may be of a different package format than the previous
//! release reads, so the test takes each digest from the raw bytes and each
//! descriptor from a plain zstd and tar decode, and never hands an archive to
//! the strict canonical reader. After the upgrade the head links a small
//! package directory, so its daemon builds and installs an archive of the head's
//! own format next to the packages the sessions are pinned to.
//!
//! Results of both CLIs are read as untyped JSON and only the fields named
//! below are consulted, so a schema difference between the releases cannot
//! fail the test before the behavior it exercises does.

#![cfg(target_os = "linux")]

// Rust guideline compliant 2026-10-12

use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use package::archive_digest;
use pohunek_service_config::preflight::{CODE_ADOPTABLE, CODE_ADOPTABLE_BY_NEWER_PACKAGE};
use pohunek_test_support::wait::poll_until;
use serde_json::Value;

/// Source of the fake Claude agent the sessions launch.
const FAKE_AGENT: &str = include_str!("support/release_upgrade_agent.sh");

/// Source of the fake Codex agent the new Codex session launches.
const FAKE_CODEX_AGENT: &str = include_str!("support/release_upgrade_codex_agent.sh");

/// Silent reader used to verify the previous client's request deadline.
const SILENT_AGENT: &str = include_str!("support/release_upgrade_silent_agent.sh");

/// Shell the fake agents' interpreter is a copy of.
const SHELL: &str = "/bin/sh";

/// File name of the copy the fake Claude agents run under. It must contain
/// `claude`: the worker accepts a launch process for the claude hooks only when
/// its executable name does.
const CLAUDE_INTERPRETER: &str = "fake-claude-sh";

/// File name of the copy the fake Codex agent runs under; it names the
/// process as a codex launch the same way.
const CODEX_INTERPRETER: &str = "fake-codex-sh";

/// Longest interpreter line (`#!` and the path) the kernel reads. Linux reads
/// the first `BINPRM_BUF_SIZE` (256) bytes of a script, newline included, and
/// refuses to execute a script whose interpreter path does not fit.
const MAX_INTERPRETER_LINE_BYTES: usize = 255;

/// Package id of the official Claude runtime package.
const CLAUDE_PACKAGE_ID: &str = "pohunek.runtime.claude";

/// Runtime id the Claude package serves; the host profiles' base.
const CLAUDE_RUNTIME_ID: &str = "claude";

/// The release directory the head's package archives and catalog live in,
/// beside the head's binaries.
const RELEASE_RUNTIME_DIR: &str = "runtime";

/// The first release whose official runtime packages widen the version ranges
/// (`below`) past the previous ones, so an upgrade to it from an older release
/// must repin the profiles and adopt the stopped session onto the head package.
/// A build below it is an older target tree and is only checked for
/// consistency. From it on, `widens` decides per package whether the outcome is
/// required.
const RUNTIME_RANGE_RELEASE: &str = "0.35.2";

/// Version the head's xtask gives the downgrade package it writes for the
/// downgrade guard (`DOWNGRADE_VERSION` in `crates/xtask/src/test_runtime.rs`).
const DOWNGRADE_VERSION: &str = "0.0.1";

/// The Claude package archive, relative to the release's `runtime/`.
const CLAUDE_PACKAGE_ARCHIVE: &str = "packages/claude.tar.zst";

/// Package id of the official Codex runtime package.
const CODEX_PACKAGE_ID: &str = "pohunek.runtime.codex";

/// Runtime id the Codex package serves; the host profile's base.
const CODEX_RUNTIME_ID: &str = "codex";

/// The Codex package archive, relative to the release's `runtime/`.
const CODEX_PACKAGE_ARCHIVE: &str = "packages/codex.tar.zst";

/// The signed runtime catalog, relative to the release's `runtime/`.
const RUNTIME_CATALOG: &str = "runtime-catalog.json";

/// The runtime descriptor inside a package archive.
const PACKAGE_DESCRIPTOR: &str = "runtime.toml";

/// Version-probe parser whose `line` template the descriptor must declare; the
/// fake agents print that template filled with the lowest accepted release.
const VERSION_LINE_PARSER: &str = "semver-line-v1";

/// Placeholder of the release inside a `semver-line-v1` `line` template.
const VERSION_PLACEHOLDER: &str = "{version}";

/// Directory under `HOME` the head links as a probe package.
const PROBE_DIRECTORY: &str = "upgrade-probe";

/// Package id of the probe package.
const PROBE_PACKAGE_ID: &str = "pohunek.upgrade.probe";

/// Detect manifest path of the probe package.
const PROBE_DETECT_PATH: &str = "detect.toml";

/// Descriptor of the probe package: an unreserved runtime that is never
/// launched, only installed.
const PROBE_DESCRIPTOR: &str = r#"schema = 1
id = "pohunek.upgrade.probe"
version = "0.1.0"
runtime_api = 1

[runtime]
id = "upgrade-probe"
name = "Upgrade probe"
program = "upgrade-probe-agent"
args = []
detect_manifest = "detect.toml"
prompt_arg = false

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = false

[fork]
supported = false

[native_reference]
strategy = "none"
"#;

/// Detect manifest of the probe package.
const PROBE_DETECT: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;
/// File names of the fake agent scripts in the agent directory.
const FAKE_CLAUDE_SCRIPT: &str = "fake-claude.sh";
const SILENT_CLAUDE_SCRIPT: &str = "silent-claude.sh";
const FAKE_CODEX_SCRIPT: &str = "fake-codex.sh";

/// Host profile of the live session, and of the new Claude session after the
/// upgrade.
const PROFILE: &str = "fake-claude";

/// Host profile of the stopped session. It is a separate profile so the
/// stopped session reports its own native id, which no live session reports.
const STOPPED_PROFILE: &str = "stopped-claude";

/// Host profile used only for the previous client's initial-input create.
const SILENT_PROFILE: &str = "silent-claude";

/// Host profile of the Codex agent, launched after the upgrade.
const CODEX_PROFILE: &str = "fake-codex";

/// Column counts of the resizes the previous release's daemon applies to the
/// live session. Its worker keeps the highest resize sequence it accepted, so
/// more than one is needed to outrun the first resize of the head's daemon.
const RESIZES_BEFORE_UPGRADE_COLS: [u16; 2] = [90, 91];

/// Size the head's daemon resizes the adopted live session to.
const RESIZE_AFTER_UPGRADE: (u16, u16) = (110, 40);

/// Rows of every resize before the upgrade.
const ROWS_BEFORE_UPGRADE: u16 = 30;

/// Native id the live session's launch reports through the previous release's
/// hook.
const NATIVE_BEFORE: &str = "native-before-upgrade";

/// Native id the stopped session reports before it is stopped.
const NATIVE_STOPPED: &str = "native-stopped-session";

/// Id reported after the upgrade. The new daemon persists this later report
/// as the current native conversation for a verified launch process.
const NATIVE_AFTER: &str = "native-after-upgrade";

/// Hook event ids of the notifications the test waits for; each carries a
/// distinct one so the test can tell which phase delivered it. The fake agent
/// also sends one at its first launch (`evt-before-upgrade`); the previous
/// release's own notification hook is not accepted by its own daemon, so that
/// one is not observable and not asserted.
const EVENT_AFTER_UPGRADE: &str = "evt-after-upgrade";
/// Prefix of the hook event id a resumed launch sends; the fake agent appends
/// its process id, because the daemon merges notifications by event id across
/// sessions.
const EVENT_AFTER_RESUME: &str = "evt-after-resume-";

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

/// Inputs and paths of one run.
struct Run {
    home: PathBuf,
    previous_dir: PathBuf,
    head_dir: PathBuf,
    artifacts: PathBuf,
    /// The head's downgrade catalog and archives, when the head's xtask wrote
    /// them; `None` skips the downgrade guard.
    downgrade: Option<PathBuf>,
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
        // Optional: `scripts/upgrade-test` sets it only when the head's xtask
        // has `--downgrade`. A head older than that writes no downgrade catalog.
        let downgrade = std::env::var_os("POHUNEK_UPGRADE_DOWNGRADE_DIR").map(|value| {
            let dir = PathBuf::from(value);
            assert!(
                dir.is_absolute(),
                "POHUNEK_UPGRADE_DOWNGRADE_DIR must be absolute"
            );
            dir
        });
        Self {
            home: path("HOME"),
            previous_dir: path("POHUNEK_PREVIOUS_BIN_DIR"),
            head_dir: path("POHUNEK_HEAD_BIN_DIR"),
            artifacts: path("POHUNEK_UPGRADE_ARTIFACTS_DIR"),
            downgrade,
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

/// Resizes `id` through `cli` and returns the `(cols, rows)` its result reports
/// the session to have.
fn resize(cli: &Cli, id: &str, cols: u16, rows: u16) -> (u64, u64) {
    let result = cli.json(&[
        "session",
        "resize",
        id,
        "--cols",
        &cols.to_string(),
        "--rows",
        &rows.to_string(),
        "--json",
    ]);
    let dimension = |name: &str| {
        result["session"][name]
            .as_u64()
            .unwrap_or_else(|| panic!("{}: resize result has no {name}: {result}", cli.label))
    };
    (dimension("cols"), dimension("rows"))
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

/// The number of notifications of session `id` that mention `event`.
fn notification_count(cli: &Cli, id: &str, event: &str) -> usize {
    let list = cli.json(&["notifications", "list", "--session", id, "--json"]);
    list["notifications"].as_array().map_or(0, |records| {
        records
            .iter()
            .filter(|record| record.to_string().contains(event))
            .count()
    })
}

/// Waits until session `id` has more than `known` notifications that mention
/// `event`.
fn wait_new_notification(cli: &Cli, id: &str, event: &str, known: usize) {
    poll_until(
        &format!("a new notification {event} of {id} via {}", cli.label),
        || {
            let list = cli
                .try_json(&["notifications", "list", "--session", id, "--json"])
                .ok()?;
            (list
                .get("notifications")?
                .as_array()?
                .iter()
                .filter(|record| record.to_string().contains(event))
                .count()
                > known)
                .then_some(())
        },
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

/// The version probe a package descriptor declares: the release range
/// `[min, below)` it accepts and the `--version` line template it parses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VersionProbe {
    min: String,
    below: String,
    line: String,
}

impl VersionProbe {
    /// The `--version` line of release `version`, filled into the template.
    fn line_of_release(&self, version: &str) -> String {
        assert_eq!(
            self.line.matches(VERSION_PLACEHOLDER).count(),
            1,
            "the version line template names the release once: {}",
            self.line
        );
        self.line.replace(VERSION_PLACEHOLDER, version)
    }

    /// The line the fake agent answers when it is the lowest accepted release.
    fn lowest_line(&self) -> String {
        self.line_of_release(&self.min)
    }

    /// Whether the probe accepts release `version`: `min <= version < below`.
    fn accepts(&self, version: &str) -> bool {
        release_order(&self.min, version) != std::cmp::Ordering::Greater
            && release_order(version, &self.below) == std::cmp::Ordering::Less
    }
}

/// The version and version probe of a package descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Descriptor {
    version: String,
    probe: VersionProbe,
}

/// Orders two releases of the form `major.minor.patch`, the form the package
/// descriptors and the binaries' `--version` use. The daemon orders package
/// versions by semver; the test needs only this plain numeric order.
fn release_order(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |text: &str| -> Vec<u64> {
        text.split('.')
            .map(|part| {
                part.parse()
                    .unwrap_or_else(|_| panic!("{text} is not a major.minor.patch release"))
            })
            .collect()
    };
    parse(left).cmp(&parse(right))
}

/// The `major.minor.patch` core of `version`. A head of the previous release's
/// version is stamped with build metadata (`+upgrade.<commit>`), which orders
/// by its core version.
fn core_version(version: &str) -> &str {
    version
        .split_once('+')
        .map_or(version, |(core, _metadata)| core)
}

/// Whether the head's package `release` must replace the previous release's
/// package `installed`: the head is `RUNTIME_RANGE_RELEASE` or later
/// (`head_is_release`), ships another digest, and either the previous release
/// predates `RUNTIME_RANGE_RELEASE` (`previous_predates_ranges`) or the head's
/// range accepts the previous range's exclusive `below` bound. The same digest
/// never widens anything, so the upgrade keeps the pins.
fn widens(
    head_is_release: bool,
    previous_predates_ranges: bool,
    installed: &Installed,
    release: &ReleasePackage,
) -> bool {
    head_is_release
        && installed.digest != release.digest
        && (previous_predates_ranges
            || release
                .descriptor
                .probe
                .accepts(&installed.descriptor.probe.below))
}

/// Reads the version and version probe of the descriptor `text` of
/// `package_id`.
fn descriptor_of(text: &str, package_id: &str) -> Descriptor {
    let descriptor: toml::Table = toml::from_str(text).expect("the descriptor is TOML");
    assert_eq!(
        descriptor.get("id").and_then(toml::Value::as_str),
        Some(package_id),
        "the descriptor is not {package_id}"
    );
    let version = descriptor
        .get("version")
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("{package_id} declares no version"))
        .to_owned();
    let probe = descriptor
        .get("runtime")
        .and_then(|runtime| runtime.get("version_probe"))
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("{package_id} declares no version probe"));
    let field = |name: &str| {
        probe
            .get(name)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("the version probe of {package_id} has no `{name}`"))
    };
    assert_eq!(
        field("parser"),
        VERSION_LINE_PARSER,
        "the fake agents answer only a line-template version probe"
    );
    Descriptor {
        version,
        probe: VersionProbe {
            min: field("min").to_owned(),
            below: field("below").to_owned(),
            line: field("line").to_owned(),
        },
    }
}

/// An official package of this build's release, read from its archive so the
/// test knows the digest the upgrade must install and the version probe its
/// fake agent must answer.
struct ReleasePackage {
    /// Digest of the archive bytes, `sha256:<hex>`.
    digest: String,
    descriptor: Descriptor,
}

impl ReleasePackage {
    /// Reads the package `archive` of the head's `runtime/` directory. The
    /// digest is taken from the raw bytes, and the descriptor from a plain zstd
    /// and tar decode; the archive is never handed to the strict reader.
    fn read(runtime_dir: &Path, archive: &str, package_id: &str) -> Self {
        let archive = runtime_dir.join(archive);
        let bytes = fs::read(&archive)
            .unwrap_or_else(|error| panic!("read {}: {error}", archive.display()));
        let digest = archive_digest(&bytes).expect("the archive digest");
        let contents = tar_member(&zstd_decode(&bytes), PACKAGE_DESCRIPTOR)
            .unwrap_or_else(|| panic!("{} has no {PACKAGE_DESCRIPTOR}", archive.display()));
        let text = std::str::from_utf8(&contents).expect("the descriptor is UTF-8");
        Self {
            digest: digest.as_str().to_owned(),
            descriptor: descriptor_of(text, package_id),
        }
    }

    /// What `--version` prints for the lowest release this package accepts.
    fn version_line(&self) -> String {
        self.descriptor.probe.lowest_line()
    }
}

/// An official package the previous release installed, read from its own
/// package store.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Installed {
    /// Archive digest, `sha256:<hex>`.
    digest: String,
    descriptor: Descriptor,
}

/// The descriptor of the package `digest` of `package_id`, read from the
/// package store the previous release installed it into.
fn installed_descriptor(run: &Run, digest: &str, package_id: &str) -> Descriptor {
    let hex = digest
        .strip_prefix("sha256:")
        .unwrap_or_else(|| panic!("{digest} is not a sha256 digest"));
    let path = run
        .state_dir()
        .join("plugins/packages")
        .join(hex)
        .join("files")
        .join(PACKAGE_DESCRIPTOR);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    descriptor_of(&text, package_id)
}

/// Decodes one zstd frame, of whichever encoder wrote it.
fn zstd_decode(bytes: &[u8]) -> Vec<u8> {
    use std::io::Read as _;
    let mut source = bytes;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut source).expect("a zstd frame");
    let mut tar = Vec::new();
    decoder
        .read_to_end(&mut tar)
        .expect("decode the zstd frame");
    tar
}

/// USTAR header block size.
const TAR_BLOCK: usize = 512;

/// Byte range of a USTAR header's name field.
const TAR_NAME: std::ops::Range<usize> = 0..100;

/// Byte range of a USTAR header's octal size field.
const TAR_SIZE: std::ops::Range<usize> = 124..136;

/// The contents of the regular file `name` in the USTAR stream `tar`, if it
/// has one. The archive's digest is authorized by its signed catalog, which
/// the previous release checks when it installs it; this only reads one file.
fn tar_member(tar: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut offset = 0;
    while offset + TAR_BLOCK <= tar.len() {
        let header = &tar[offset..offset + TAR_BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            return None;
        }
        let field = &header[TAR_NAME];
        let path = &field[..field
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(field.len())];
        let size = std::str::from_utf8(&header[TAR_SIZE])
            .ok()
            .and_then(|text| usize::from_str_radix(text.trim_matches(['\0', ' ']), 8).ok())?;
        let start = offset + TAR_BLOCK;
        let end = start.checked_add(size).filter(|end| *end <= tar.len())?;
        if path == name.as_bytes() {
            return Some(tar[start..end].to_vec());
        }
        offset = start + size.div_ceil(TAR_BLOCK) * TAR_BLOCK;
    }
    None
}

/// Writes `source` as an owner-only executable script interpreted by
/// `interpreter`, defining the `FAKE_AGENT_VERSION_LINE` its `--version`
/// answer prints.
fn write_agent_script(path: &Path, source: &str, interpreter: &Path, version_line: &str) {
    let (first, body) = source.split_once('\n').expect("an agent script has a body");
    assert!(
        first.starts_with("#!"),
        "an agent script starts with an interpreter line"
    );
    let interpreter = interpreter.to_str().expect("the interpreter path is UTF-8");
    let interpreter_line = format!("#!{interpreter}");
    assert!(
        interpreter_line.len() <= MAX_INTERPRETER_LINE_BYTES
            && !interpreter.contains(char::is_whitespace),
        "the kernel cannot run an interpreter at {interpreter}: \
         the line is too long or the path has whitespace"
    );
    // The line is assigned single-quoted, so it must contain no quote and no
    // line break.
    assert!(
        !version_line.contains(['\'', '\n']),
        "the version line cannot be quoted for the shell: {version_line}"
    );
    let text = format!("{interpreter_line}\nFAKE_AGENT_VERSION_LINE='{version_line}'\n{body}");
    // The script is replaced by a rename: a shell running the old text reads
    // its script by offset, so an in-place rewrite would feed it the new text.
    let staged = path.with_extension("next");
    fs::write(&staged, text).unwrap_or_else(|error| panic!("write {}: {error}", staged.display()));
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("chmod {}: {error}", staged.display()));
    fs::rename(&staged, path).unwrap_or_else(|error| panic!("replace {}: {error}", path.display()));
}

/// Copies `shell` to `name` in the agent directory, as an owner-only
/// executable, and returns the copy's path.
fn copy_interpreter(run: &Run, shell: &Path, name: &str) -> PathBuf {
    let interpreter = run.agent_dir().join(name);
    fs::copy(shell, &interpreter).expect("copy the shell as an agent interpreter");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700))
        .expect("chmod the agent interpreter");
    interpreter
}

/// Writes the host profile `name` in the config directory's `agents`.
fn write_profile(run: &Run, name: &str, text: &str) {
    let path = run.config_dir().join("agents").join(format!("{name}.toml"));
    fs::write(&path, text).expect("write the host profile");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod the profile");
}

/// Writes the fake Claude agents' scripts, which answer `--version` with
/// `claude_line`.
fn write_claude_scripts(run: &Run, claude_line: &str) {
    let claude_interpreter = run.agent_dir().join(CLAUDE_INTERPRETER);
    write_agent_script(
        &run.agent_dir().join(FAKE_CLAUDE_SCRIPT),
        FAKE_AGENT,
        &claude_interpreter,
        claude_line,
    );
    write_agent_script(
        &run.agent_dir().join(SILENT_CLAUDE_SCRIPT),
        SILENT_AGENT,
        &claude_interpreter,
        claude_line,
    );
}

/// Writes the fake agents' scripts. Each answers `--version` with its version
/// line, which the probe of the package it runs under accepts.
fn write_agent_scripts(run: &Run, claude_line: &str, codex_line: &str) {
    let codex_interpreter = run.agent_dir().join(CODEX_INTERPRETER);
    write_claude_scripts(run, claude_line);
    write_agent_script(
        &run.agent_dir().join(FAKE_CODEX_SCRIPT),
        FAKE_CODEX_AGENT,
        &codex_interpreter,
        codex_line,
    );
}

/// Writes the fake agents, their host profiles, and the Claude config directory
/// the managed hooks are installed into. The profiles name the built-in runtimes
/// as their base and carry no package pin.
fn prepare_agent(run: &Run, claude_line: &str, codex_line: &str) {
    let private = |path: &Path| {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    };
    private(&run.agent_dir());
    private(&run.home.join(".claude"));
    let shell = fs::canonicalize(SHELL).expect("resolve the shell");
    copy_interpreter(run, &shell, CLAUDE_INTERPRETER);
    copy_interpreter(run, &shell, CODEX_INTERPRETER);
    write_agent_scripts(run, claude_line, codex_line);
    let script = run.agent_dir().join(FAKE_CLAUDE_SCRIPT);
    let silent_script = run.agent_dir().join(SILENT_CLAUDE_SCRIPT);
    let codex_script = run.agent_dir().join(FAKE_CODEX_SCRIPT);

    let agents = run.config_dir().join("agents");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&agents)
        .unwrap_or_else(|error| panic!("create {}: {error}", agents.display()));
    // Input is pasted as plain text: the fake agent reads lines with `read`
    // and would see the bracketed-paste escapes otherwise.
    let claude_profile = |native_id: &str| {
        format!(
            "base = \"{CLAUDE_RUNTIME_ID}\"\nprogram = \"{}\"\n[env]\nFAKE_AGENT_LOG = \"{}\"\n\
             FAKE_AGENT_HOOKS = \"{}\"\nFAKE_AGENT_NATIVE_ID = \"{native_id}\"\n\
             [input_rules]\nbracketed_paste = false\n",
            script.display(),
            run.agent_log().display(),
            run.claude_hooks().display(),
        )
    };
    write_profile(run, PROFILE, &claude_profile(NATIVE_BEFORE));
    write_profile(run, STOPPED_PROFILE, &claude_profile(NATIVE_STOPPED));
    write_profile(
        run,
        SILENT_PROFILE,
        &format!(
            "base = \"{CLAUDE_RUNTIME_ID}\"\nprogram = \"{}\"\n[input_rules]\nbracketed_paste = false\n",
            silent_script.display(),
        ),
    );
    write_profile(
        run,
        CODEX_PROFILE,
        &format!(
            "base = \"{CODEX_RUNTIME_ID}\"\nprogram = \"{}\"\n[input_rules]\nbracketed_paste = false\n",
            codex_script.display(),
        ),
    );
}

/// Where the metadata store keeps a copy of a session's launch binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingCopy {
    /// `recovery.launch_binding` of the session's `kind: "session"` record.
    SessionRecovery,
    /// `launch_binding` of the session's `kind: "resume"` record.
    ResumeRecord,
}

/// The copies a session that reported its native id and is live requires. The
/// session record carries the recovery binding the session was launched with.
/// The resume record exists while the session is live and has reported its
/// native reference: the daemon upserts it after the report and removes it when
/// the session ends (`persist_resume_binding` in
/// `crates/daemon/src/session/resume.rs`, the same in v0.34.2).
const REQUIRED_COPIES: [BindingCopy; 2] = [BindingCopy::SessionRecovery, BindingCopy::ResumeRecord];

/// The copy a stopped session or a session that never reported requires: its
/// session record. The resume record is absent or not checked for these.
const SESSION_COPY: [BindingCopy; 1] = [BindingCopy::SessionRecovery];

/// Every stored copy of the launch binding of session `id` in the store text,
/// with `None` for a copy whose binding is absent.
///
/// The store serializes an unpinned binding by omitting `launch_binding`, so a
/// copy is identified by its record kind, never by the key: a recovery or resume
/// record without the key is a copy that lost its pin. Lines are read as
/// untyped JSON, so a schema difference between the releases changes nothing
/// as long as the record kinds and these fields keep their spelling.
fn parse_launch_bindings(
    text: &str,
    id: &str,
) -> Result<Vec<(BindingCopy, Option<Value>)>, String> {
    let mut copies = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line)
            .map_err(|error| format!("line {}: not JSON: {error}", number + 1))?;
        if record.get("session_id").and_then(Value::as_str) != Some(id) {
            continue;
        }
        match record.get("kind").and_then(Value::as_str) {
            Some("session") => {
                if let Some(recovery) = record.get("recovery").filter(|value| !value.is_null()) {
                    copies.push((
                        BindingCopy::SessionRecovery,
                        recovery.get("launch_binding").cloned(),
                    ));
                }
            }
            Some("resume") => {
                copies.push((
                    BindingCopy::ResumeRecord,
                    record.get("launch_binding").cloned(),
                ));
            }
            _ => {}
        }
    }
    Ok(copies)
}

/// The provenance a stored launch binding must carry.
#[derive(Debug, Clone, Copy)]
enum Pin<'a> {
    /// The built-in runtime whose descriptor stands for `package_id`.
    Builtin { package_id: &'a str },
    /// The package `package_id` installed with archive digest `digest`.
    Package {
        package_id: &'a str,
        digest: &'a str,
    },
}

/// Whether `copies` holds each of `required` exactly once and every copy binds
/// `runtime_id` as `expected`; the error names the first violation.
fn check_pins(
    copies: &[(BindingCopy, Option<Value>)],
    required: &[BindingCopy],
    runtime_id: &str,
    expected: Pin<'_>,
) -> Result<(), String> {
    for copy in required {
        let count = copies.iter().filter(|(found, _)| found == copy).count();
        if count != 1 {
            return Err(format!("{count} {copy:?} copies, expected one"));
        }
    }
    for (copy, binding) in copies {
        let Some(binding) = binding else {
            return Err(format!("the {copy:?} copy has no launch binding"));
        };
        let provenance = &binding["provenance"];
        let pinned = binding["runtime_id"] == runtime_id
            && match expected {
                Pin::Builtin { package_id } => {
                    provenance["kind"] == "builtin" && provenance["package"]["id"] == package_id
                }
                Pin::Package { package_id, digest } => {
                    provenance["kind"] == "package"
                        && provenance["package"]["id"] == package_id
                        && provenance["package_digest"] == digest
                }
            };
        if !pinned {
            return Err(format!(
                "the {copy:?} copy is not {expected:?} of {runtime_id}: {binding}"
            ));
        }
    }
    Ok(())
}

/// The store binds session `id` to `expected` in each of `required` copies,
/// and every copy it holds binds `runtime_id` the same way.
///
/// A resume record is written after the native report the caller waited for,
/// so the check waits until each required copy is present, then judges the
/// copies once: a copy that is present but unpinned or pinned elsewhere fails
/// at once.
fn assert_store_pin(
    run: &Run,
    id: &str,
    runtime_id: &str,
    expected: Pin<'_>,
    required: &[BindingCopy],
    phase: &str,
) {
    let path = run.data_dir().join("metadata.jsonl");
    let read = || {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{phase}: read {}: {error}", path.display()));
        parse_launch_bindings(&text, id)
            .unwrap_or_else(|error| panic!("{phase}: {}: {error}", path.display()))
    };
    let copies = poll_until(&format!("{phase}: stored launch bindings of {id}"), || {
        let copies = read();
        required
            .iter()
            .all(|needed| copies.iter().any(|(copy, _)| copy == needed))
            .then_some(copies)
    });
    if let Err(violation) = check_pins(&copies, required, runtime_id, expected) {
        panic!("{phase}: {id}: {violation}: {copies:?}");
    }
}

/// The adoption preflight in a `service check` or `service upgrade` payload
/// was run by this build's daemon. It judges the live session `live` adoptable
/// with code `adoptable`, and the stopped session `stopped` adoptable with code
/// `stopped_code`, which is `adoptable_by_newer_package` when the stopped session
/// resumes onto a newer package. No session is at risk.
/// The daemon version rules out the previous daemon's rollback verdict, which
/// the same field carries for a rollback.
fn assert_preflight_adoptable(
    payload: &Value,
    live: &str,
    stopped: &str,
    stopped_code: &str,
    head_version: &str,
    operation: &str,
) {
    assert_eq!(
        payload.pointer("/preflight/daemon_version"),
        Some(&Value::from(head_version)),
        "{operation}: the preflight was not run by this build's daemon: {payload}"
    );
    eprintln!(
        "{operation} preflight verdicts: live {live} adoptable, stopped {stopped} {stopped_code}"
    );
    for (list, id, code) in [
        ("sessions", live, CODE_ADOPTABLE),
        ("stopped_sessions", stopped, stopped_code),
    ] {
        let sessions = payload
            .pointer(&format!("/preflight/{list}"))
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{operation} carries no preflight {list}: {payload}"));
        let session = sessions
            .iter()
            .find(|session| session["session_id"] == id)
            .unwrap_or_else(|| {
                panic!("{operation}: the preflight does not judge {id} in {list}: {payload}")
            });
        assert_eq!(
            session["verdict"], "adoptable",
            "{operation}: {id} in {list} is not adoptable: {session}"
        );
        assert_eq!(
            session["code"], code,
            "{operation}: {id} in {list} carries the wrong code: {session}"
        );
        assert!(
            sessions
                .iter()
                .all(|session| session["verdict"] == "adoptable"),
            "{operation}: the preflight puts a session at risk in {list}: {payload}"
        );
    }
}

/// Checks one official runtime step, a plan or an applied report: no refusal,
/// exactly one package entry per `(runtime, digest, status)`, and exactly the
/// profile entries of `profiles`, each as `(name, status)`.
fn assert_runtime_step(
    step: &Value,
    packages: &[(&str, &str, &str)],
    profiles: &[(&str, &str)],
    operation: &str,
) {
    assert_eq!(
        step["refusals"],
        Value::Array(Vec::new()),
        "{operation}: the runtime step refuses: {step}"
    );
    let list = |name: &str| {
        step[name]
            .as_array()
            .unwrap_or_else(|| panic!("{operation} carries no {name}: {step}"))
    };
    for (runtime, digest, status) in packages {
        let action = list("packages")
            .iter()
            .find(|action| action["runtime"] == *runtime)
            .unwrap_or_else(|| panic!("{operation}: no package step for {runtime}: {step}"));
        assert_eq!(action["status"], *status, "{operation}: {action}");
        assert_eq!(action["digest"], *digest, "{operation}: {action}");
    }
    assert_eq!(
        list("packages").len(),
        packages.len(),
        "{operation}: unexpected package steps: {step}"
    );
    for (name, status) in profiles {
        let action = list("profiles")
            .iter()
            .find(|action| action["name"] == *name)
            .unwrap_or_else(|| panic!("{operation}: no profile step for {name}: {step}"));
        assert_eq!(action["status"], *status, "{operation}: {action}");
    }
    assert_eq!(
        list("profiles").len(),
        profiles.len(),
        "{operation}: unexpected profile steps: {step}"
    );
    eprintln!("{operation} runtime step: {step}");
}

/// Creates a session of `profile` through `cli` and returns its id.
fn new_session(cli: &Cli, profile: &str, name: &str, cwd: &str) -> String {
    let created = cli.json(&[
        "session", "new", "--agent", profile, "--name", name, "--cwd", cwd, "--json",
    ]);
    created
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("session new returned no id: {created}"))
        .to_owned()
}

/// Stops session `id` through `cli`, waits until its runtime has ended, and
/// returns the identity it had before the stop.
fn stop_session(cli: &Cli, id: &str) -> Runtime {
    let before = wait_live(cli, id);
    let stopped = cli.json(&["session", "stop", id, "--json"]);
    assert_eq!(
        stopped["stopped"], true,
        "stop did not stop the session: {stopped}"
    );
    poll_until(&format!("{id} runtime ended"), || {
        runtime(&inspect(cli, id)?)
            .filter(|runtime| matches!(runtime.state.as_str(), "terminal" | "lost"))
    });
    before
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

/// The official packages of this build's release, read from its `runtime/`.
struct Packages {
    claude: ReleasePackage,
    codex: ReleasePackage,
}

/// What the previous release started the host on, read from its own
/// `plugin profile list --json` after its `service install`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Start {
    /// The built-in runtimes, no profile pinned. The upgrade installs the head's
    /// packages and pins every profile to them.
    Builtin,
    /// The previous release's own packages, which its `service install`
    /// installed and pinned the profiles to. The upgrade installs the head's
    /// packages beside them. A newer head version repins the profiles to the
    /// head's digest; the same digest keeps the pins.
    Package {
        claude: Box<Installed>,
        codex: Box<Installed>,
    },
}

/// Reads the start state from the previous release's profile listing. Every
/// test profile must agree: all pinned to a package, or none pinned.
fn read_start(run: &Run, previous: &Cli) -> Start {
    let listed = previous.json(&["plugin", "profile", "list", "--json"]);
    let profiles = listed["profiles"]
        .as_array()
        .unwrap_or_else(|| panic!("the profile listing carries no profiles: {listed}"));
    let entry = |name: &str| {
        profiles
            .iter()
            .find(|profile| profile["name"] == name)
            .unwrap_or_else(|| panic!("the profile listing has no {name}: {listed}"))
    };
    let names = [PROFILE, STOPPED_PROFILE, SILENT_PROFILE, CODEX_PROFILE];
    let pinned = names.map(|name| entry(name)["state"] == "pinned");
    if pinned.iter().all(|pinned| !pinned) {
        return Start::Builtin;
    }
    assert!(
        pinned.iter().all(|pinned| *pinned),
        "the previous release pinned some test profiles and not others: {listed}"
    );
    let digest = |name: &str| {
        entry(name)["digest"]
            .as_str()
            .unwrap_or_else(|| panic!("the pinned profile {name} names no digest: {listed}"))
            .to_owned()
    };
    let claude = digest(PROFILE);
    assert_eq!(
        digest(STOPPED_PROFILE),
        claude,
        "the Claude profiles pin different packages: {listed}"
    );
    assert_eq!(
        digest(SILENT_PROFILE),
        claude,
        "the Claude profiles pin different packages: {listed}"
    );
    let codex = digest(CODEX_PROFILE);
    Start::Package {
        claude: Box::new(Installed {
            descriptor: installed_descriptor(run, &claude, CLAUDE_PACKAGE_ID),
            digest: claude,
        }),
        codex: Box::new(Installed {
            descriptor: installed_descriptor(run, &codex, CODEX_PACKAGE_ID),
            digest: codex,
        }),
    }
}

/// The digest a session launched on `start` carries for `package_id` before the
/// upgrade.
fn start_pin<'a>(start: &'a Start, package_id: &'static str) -> Pin<'a> {
    match start {
        Start::Builtin => Pin::Builtin { package_id },
        Start::Package { claude, codex } => Pin::Package {
            package_id,
            digest: if package_id == CLAUDE_PACKAGE_ID {
                &claude.digest
            } else {
                &codex.digest
            },
        },
    }
}

/// Both releases, the versions they report, the sessions under test, and the
/// head's packages.
struct Upgrade {
    run: Run,
    previous: Cli,
    head: Cli,
    previous_version: String,
    head_version: String,
    /// Whether the head's build is the release that widens the official
    /// runtime ranges (0.35.2 or later). From it on the downgrade guard is
    /// required, and `widens` decides which packages must repin.
    head_is_release: bool,
    /// Whether the previous release predates `RUNTIME_RANGE_RELEASE`, so its
    /// official packages carry the narrower ranges.
    previous_predates_ranges: bool,
    /// The live session of the Claude profile; the upgrade adopts it.
    id: String,
    /// The stopped session of the Claude profile; it resumes onto its pin.
    stopped_id: String,
    packages: Packages,
    start: Start,
}

/// The runtime identities and the daemon the previous release leaves behind.
struct Before {
    /// The live session's identity before the upgrade.
    live: Runtime,
    /// The stopped session's identity when it stopped.
    stopped: Runtime,
    /// Pid of the previous release's daemon.
    daemon: u64,
}

/// The previous release installs its managed Claude hooks, starts a stopped and
/// a live session of the Claude profiles, and leaves them there. Its profiles
/// are pinned when its own install pinned them, and built-in otherwise.
fn start_on_previous(run: Run, previous: Cli, head: Cli) -> (Upgrade, Before) {
    let (previous_version, head_version) = (previous.version(), head.version());
    assert_ne!(
        previous_version, head_version,
        "an upgrade to the same version is a no-op and proves nothing"
    );
    let head_is_release = release_order(core_version(&head_version), RUNTIME_RANGE_RELEASE)
        != std::cmp::Ordering::Less;
    let previous_predates_ranges =
        release_order(core_version(&previous_version), RUNTIME_RANGE_RELEASE)
            == std::cmp::Ordering::Less;
    eprintln!(
        "upgrade {previous_version} -> {head_version} (runtime range release: {head_is_release}, \
         previous predates it: {previous_predates_ranges})"
    );
    let release = run.head_dir.join(RELEASE_RUNTIME_DIR);
    assert!(
        release.join(RUNTIME_CATALOG).is_file(),
        "the head ships no {}",
        release.join(RUNTIME_CATALOG).display()
    );
    let packages = Packages {
        claude: ReleasePackage::read(&release, CLAUDE_PACKAGE_ARCHIVE, CLAUDE_PACKAGE_ID),
        codex: ReleasePackage::read(&release, CODEX_PACKAGE_ARCHIVE, CODEX_PACKAGE_ID),
    };
    eprintln!(
        "head packages: claude {} answers `{}`, codex {} answers `{}`",
        packages.claude.digest,
        packages.claude.version_line(),
        packages.codex.digest,
        packages.codex.version_line()
    );
    // The profiles exist before the install, so an install that pins them does.
    prepare_agent(
        &run,
        &packages.claude.version_line(),
        &packages.codex.version_line(),
    );
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
    let start = read_start(&run, &previous);
    if let Start::Package { claude, codex } = &start {
        // The previous release's probe answers the version line of the package
        // it installed, which may differ from the head's.
        write_agent_scripts(
            &run,
            &claude.descriptor.probe.lowest_line(),
            &codex.descriptor.probe.lowest_line(),
        );
    }
    eprintln!("previous release starts on {start:?}");
    let sessions = start_sessions(&run, &previous, &start);
    let daemon = daemon_pid(&service_status(&previous));
    let upgrade = Upgrade {
        run,
        previous,
        head,
        previous_version,
        head_version,
        id: sessions.id,
        stopped_id: sessions.stopped_id,
        packages,
        start,
        head_is_release,
        previous_predates_ranges,
    };
    let before = Before {
        live: sessions.live,
        stopped: sessions.stopped,
        daemon,
    };
    (upgrade, before)
}

/// Starts the stopped session and the live session on the previous release and
/// returns their ids and identities. The stopped session runs first, so its
/// native id is the only one it reports while it is live.
fn start_sessions(run: &Run, previous: &Cli, start: &Start) -> Sessions {
    let cwd = run.home.display().to_string();

    // The stopped session runs first, so its native id is the only one it
    // reports while it is live.
    let stopped_id = new_session(previous, STOPPED_PROFILE, "upgrade-stopped", &cwd);
    wait_screen(previous, &stopped_id, "fake-agent ready");
    wait_native_id(previous, &stopped_id, NATIVE_STOPPED);
    assert_store_pin(
        run,
        &stopped_id,
        CLAUDE_RUNTIME_ID,
        start_pin(start, CLAUDE_PACKAGE_ID),
        &REQUIRED_COPIES,
        "before the upgrade: stopped session",
    );
    let stopped = stop_session(previous, &stopped_id);
    assert_store_pin(
        run,
        &stopped_id,
        CLAUDE_RUNTIME_ID,
        start_pin(start, CLAUDE_PACKAGE_ID),
        &SESSION_COPY,
        "before the upgrade: stopped session after its stop",
    );

    let id = new_session(previous, PROFILE, "upgrade-fake", &cwd);
    let live = wait_live(previous, &id);
    wait_screen(previous, &id, "fake-agent ready");
    wait_native_id(previous, &id, NATIVE_BEFORE);
    let launches = run.wait_launches(2);
    assert_eq!(
        launches.len(),
        2,
        "one launch per session before the upgrade: {launches:?}"
    );
    assert_store_pin(
        run,
        &id,
        CLAUDE_RUNTIME_ID,
        start_pin(start, CLAUDE_PACKAGE_ID),
        &REQUIRED_COPIES,
        "before the upgrade: live session",
    );
    Sessions {
        stopped_id,
        stopped,
        id,
        live,
    }
}

/// The sessions the previous release leaves behind, with their identities.
struct Sessions {
    stopped_id: String,
    stopped: Runtime,
    id: String,
    live: Runtime,
}

/// Removes the probe package directory `dir`, if it exists.
fn remove_probe(dir: &Path) {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", dir.display()),
    }
}

impl Upgrade {
    /// The official package of `package_id` this build's release ships.
    fn release(&self, package_id: &str) -> &ReleasePackage {
        if package_id == CLAUDE_PACKAGE_ID {
            &self.packages.claude
        } else {
            &self.packages.codex
        }
    }

    /// The package of `package_id` the previous release installed, `None` for a
    /// built-in start.
    fn installed(&self, package_id: &str) -> Option<&Installed> {
        match &self.start {
            Start::Builtin => None,
            Start::Package { claude, codex } => Some(if package_id == CLAUDE_PACKAGE_ID {
                claude
            } else {
                codex
            }),
        }
    }

    /// Whether the head's package of `package_id` must replace the one the
    /// previous release installed (see `widens`); never for a built-in start.
    fn must_widen(&self, package_id: &str) -> bool {
        self.installed(package_id).is_some_and(|installed| {
            widens(
                self.head_is_release,
                self.previous_predates_ranges,
                installed,
                self.release(package_id),
            )
        })
    }

    /// Whether the upgrade repins the profiles of `package_id`: the head's
    /// package is a different digest of a strictly newer version than the one
    /// the previous release installed.
    fn repins(&self, package_id: &str) -> bool {
        self.installed(package_id).is_some_and(|installed| {
            let release = self.release(package_id);
            installed.digest != release.digest
                && release_order(&release.descriptor.version, &installed.descriptor.version)
                    == std::cmp::Ordering::Greater
        })
    }

    /// The digest the test profiles name after the upgrade: the head's package
    /// for a built-in start (the upgrade pins them) and for a repinned package;
    /// the installed package's digest otherwise.
    fn profile_digest(&self, package_id: &str) -> &str {
        match self.installed(package_id) {
            Some(installed) if !self.repins(package_id) => &installed.digest,
            _ => &self.release(package_id).digest,
        }
    }

    /// The profiles the test pins, each with its package id and digest after
    /// the upgrade.
    fn pinned_profiles(&self) -> [(&'static str, &'static str, &str); 4] {
        [
            (
                PROFILE,
                CLAUDE_PACKAGE_ID,
                self.profile_digest(CLAUDE_PACKAGE_ID),
            ),
            (
                STOPPED_PROFILE,
                CLAUDE_PACKAGE_ID,
                self.profile_digest(CLAUDE_PACKAGE_ID),
            ),
            (
                SILENT_PROFILE,
                CLAUDE_PACKAGE_ID,
                self.profile_digest(CLAUDE_PACKAGE_ID),
            ),
            (
                CODEX_PROFILE,
                CODEX_PACKAGE_ID,
                self.profile_digest(CODEX_PACKAGE_ID),
            ),
        ]
    }

    /// The profile steps of a plan (`applied` false) or of an apply (`applied`
    /// true), as `(profile name, status)`. A built-in start binds every profile.
    /// A package start repins the profiles of a repinned package and has no
    /// entry for the others: their pins already name the release digest, or the
    /// step keeps them.
    fn profile_steps(&self, applied: bool) -> Vec<(&'static str, &'static str)> {
        self.pinned_profiles()
            .iter()
            .filter_map(|&(name, package_id, _)| {
                let status = match &self.start {
                    Start::Builtin => {
                        if applied {
                            "bound"
                        } else {
                            "bind"
                        }
                    }
                    Start::Package { .. } if self.repins(package_id) => {
                        if applied {
                            "repinned"
                        } else {
                            "repin"
                        }
                    }
                    Start::Package { .. } => return None,
                };
                Some((name, status))
            })
            .collect()
    }

    /// The verdict code the stopped Claude session carries after the upgrade:
    /// the adoption of a newer package when the Claude profiles are repinned,
    /// which R2 resumes the session through; plain `adoptable` otherwise.
    fn stopped_code(&self) -> &'static str {
        if self.repins(CLAUDE_PACKAGE_ID) {
            CODE_ADOPTABLE_BY_NEWER_PACKAGE
        } else {
            CODE_ADOPTABLE
        }
    }

    /// Checks that each repinned profile of `step` moves from the installed
    /// package to the head's, with the previous digest and version named.
    fn assert_repins_named(&self, step: &Value, operation: &str) {
        for (name, package_id, _) in self.pinned_profiles() {
            if !self.repins(package_id) {
                continue;
            }
            let installed = self
                .installed(package_id)
                .expect("a repin has an installed package");
            let release = self.release(package_id);
            let action = step["profiles"]
                .as_array()
                .and_then(|actions| actions.iter().find(|action| action["name"] == name))
                .unwrap_or_else(|| panic!("{operation}: no profile step for {name}: {step}"));
            assert_eq!(
                action["previous_digest"],
                installed.digest.as_str(),
                "{operation}: {name} does not name the previous digest: {action}"
            );
            assert_eq!(
                action["previous_version"],
                installed.descriptor.version.as_str(),
                "{operation}: {name} does not name the previous version: {action}"
            );
            assert_eq!(
                action["digest"],
                release.digest.as_str(),
                "{operation}: {name} is not repinned to the head digest: {action}"
            );
            assert_eq!(
                action["version"],
                release.descriptor.version.as_str(),
                "{operation}: {name} is not repinned to the head version: {action}"
            );
            eprintln!("{operation}: profile {name}: {action}");
        }
    }

    /// The plan status of the head's package for `runtime`: `present` where the
    /// host already serves the head's digest, `install` otherwise.
    fn planned_status(&self, runtime: &str, head_digest: &str) -> &'static str {
        let package_id = if runtime == CLAUDE_RUNTIME_ID {
            CLAUDE_PACKAGE_ID
        } else {
            CODEX_PACKAGE_ID
        };
        let installed = self
            .installed(package_id)
            .map(|installed| installed.digest.as_str());
        if installed == Some(head_digest) {
            "present"
        } else {
            "install"
        }
    }

    /// The head's packages the plan names, as `(runtime, digest, plan status)`.
    fn planned_packages(&self) -> [(&'static str, &str, &'static str); 2] {
        let claude = self.packages.claude.digest.as_str();
        let codex = self.packages.codex.digest.as_str();
        [
            (
                CLAUDE_RUNTIME_ID,
                claude,
                self.planned_status(CLAUDE_RUNTIME_ID, claude),
            ),
            (
                CODEX_RUNTIME_ID,
                codex,
                self.planned_status(CODEX_RUNTIME_ID, codex),
            ),
        ]
    }

    /// Makes the fake Claude agents answer a release the previous package's
    /// version probe refuses and the head's accepts: the previous `below` bound,
    /// which is exclusive. The stopped session then cannot resume on its
    /// previous pin, so only the adoption onto the head package can resume it.
    /// The rewrite happens before the upgrade, so the previous package's probe
    /// never sees the new line before it is refused. A built-in start has no
    /// previous package range, so its agents keep their line.
    fn point_claude_agent_at_newer_range(&self) {
        let Start::Package { claude, .. } = &self.start else {
            eprintln!("the built-in start has no previous package range; the agent keeps its line");
            return;
        };
        let previous = &claude.descriptor.probe;
        let head = &self.packages.claude.descriptor.probe;
        let release = previous.below.as_str();
        assert!(
            !previous.accepts(release),
            "the previous package accepts its own bound {release}: {previous:?}"
        );
        if !head.accepts(release) {
            // The head package carries the previous range (or is the previous
            // package itself), so it refuses the bound as well. Its agent keeps
            // the lowest accepted line.
            assert!(
                !self.must_widen(CLAUDE_PACKAGE_ID),
                "the previous release {} predates runtime range release {RUNTIME_RANGE_RELEASE}, \
                 but the head's claude package {} does not accept {release}: {head:?}",
                self.previous_version,
                self.packages.claude.digest
            );
            eprintln!(
                "the head claude range {head:?} does not accept {release}: the agent keeps its line"
            );
            return;
        }
        let line = head.line_of_release(release);
        eprintln!(
            "the fake claude agents now answer `{line}`: refused by the previous range {previous:?}, \
             accepted by the head range {head:?}"
        );
        write_claude_scripts(&self.run, &line);
    }

    /// Checks that the head repins every package that must widen the
    /// previous one (see `widens`), and that a package the head ships with the
    /// previous digest is the no-op: no repin. The plan, the profile steps and
    /// the stopped session's `adoptable` verdict follow from `repins`, so the
    /// callers' checks of those assert the rest of the no-op path.
    fn assert_strict_repins(&self, operation: &str) {
        for package_id in [CLAUDE_PACKAGE_ID, CODEX_PACKAGE_ID] {
            let Some(installed) = self.installed(package_id) else {
                continue;
            };
            let release = self.release(package_id);
            if self.must_widen(package_id) {
                assert!(
                    self.repins(package_id),
                    "{operation}: the head {} must widen {package_id} of the previous {}, but it \
                     does not repin: the head {} {}, the previous {} {}",
                    self.head_version,
                    self.previous_version,
                    release.digest,
                    release.descriptor.version,
                    installed.digest,
                    installed.descriptor.version,
                );
            } else if installed.digest == release.digest {
                assert!(
                    !self.repins(package_id),
                    "{operation}: {package_id} repins onto its own digest {}",
                    release.digest
                );
                eprintln!(
                    "{operation}: the head ships the previous {package_id} {}: the pins are kept",
                    release.digest
                );
            }
        }
    }

    /// The downgrade guard: `plugin update` of the Claude package to the older
    /// archive in `dir` is refused with `plugin_update_downgrade` and writes
    /// nothing. The head's Claude digest stays the selected one, and neither the
    /// offered archive nor the downgrade version is installed.
    fn assert_downgrade_guard(&self) {
        let Some(dir) = self.run.downgrade.as_ref() else {
            assert!(
                !self.head_is_release,
                "the head is {} (runtime range release {RUNTIME_RANGE_RELEASE} or later), but \
                 POHUNEK_UPGRADE_DOWNGRADE_DIR is unset: the downgrade guard cannot run",
                self.head_version
            );
            eprintln!(
                "the downgrade guard is not checked: POHUNEK_UPGRADE_DOWNGRADE_DIR is unset, \
                 so the head's xtask writes no downgrade catalog"
            );
            return;
        };
        let archive = dir.join(CLAUDE_PACKAGE_ARCHIVE);
        let catalog = dir.join(RUNTIME_CATALOG);
        let bytes = fs::read(&archive)
            .unwrap_or_else(|error| panic!("read {}: {error}", archive.display()));
        let offered = archive_digest(&bytes).expect("the downgrade archive digest");
        let archive = archive.display().to_string();
        let catalog = catalog.display().to_string();
        let output = self.head.run(&[
            "plugin",
            "update",
            CLAUDE_PACKAGE_ID,
            &archive,
            "--catalog",
            &catalog,
            "--yes",
            "--json",
        ]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        eprintln!("plugin update of the downgrade archive: {stdout}");
        assert!(
            !output.status.success(),
            "the downgrade archive was accepted: {stdout}"
        );
        let document: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("the downgrade refusal is not JSON: {error}: {stdout}"));
        assert_eq!(
            document["err"]["code"], "plugin_update_downgrade",
            "the downgrade archive is not refused as a downgrade: {document}"
        );

        let listed = self.head.json(&["plugin", "list", "--json"]);
        eprintln!("plugin list after the refused downgrade: {listed}");
        let packages = listed["packages"]
            .as_array()
            .unwrap_or_else(|| panic!("the package listing carries no packages: {listed}"));
        let claude: Vec<&Value> = packages
            .iter()
            .filter(|package| package["package"]["id"] == CLAUDE_PACKAGE_ID)
            .collect();
        let head_digest = self.packages.claude.digest.as_str();
        assert!(
            claude.iter().any(|package| package["digest"] == head_digest
                && package["selected"] == true),
            "the head Claude digest {head_digest} is not the selected one after the refusal: {listed}"
        );
        assert!(
            !claude
                .iter()
                .any(|package| package["digest"] == offered.as_str()),
            "the refused downgrade archive {} was installed: {listed}",
            offered.as_str()
        );
        assert!(
            !claude
                .iter()
                .any(|package| package["package"]["version"] == DOWNGRADE_VERSION),
            "a {DOWNGRADE_VERSION} Claude record exists after the refused downgrade: {listed}"
        );
    }

    /// The head links a package directory, so the head daemon builds an
    /// archive of the head's package format and installs it. It installs next
    /// to the packages the sessions are pinned to, and both are listed
    /// afterwards. The probe serves an unreserved runtime, which a linked
    /// package may claim.
    fn assert_head_links_a_package_of_its_format(&self) {
        let dir = self.run.home.join(PROBE_DIRECTORY);
        // A run that stopped before its cleanup leaves the directory behind.
        remove_probe(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
        fs::write(dir.join(PACKAGE_DESCRIPTOR), PROBE_DESCRIPTOR)
            .expect("write the probe descriptor");
        fs::write(dir.join(PROBE_DETECT_PATH), PROBE_DETECT)
            .expect("write the probe detect manifest");
        let path = dir.display().to_string();
        let linked = self
            .head
            .json(&["plugin", "link", &path, "--yes", "--json"]);
        // `plugin link` copied the directory into the package store.
        remove_probe(&dir);
        let info = &linked["package"];
        assert_eq!(info["package"]["id"], PROBE_PACKAGE_ID, "{linked}");
        let digest = info["digest"].as_str().expect("the linked digest");
        assert_ne!(digest, self.packages.claude.digest, "{linked}");
        let listed = self.head.json(&["plugin", "list", "--json"]);
        let text = listed.to_string();
        for expected in [digest, self.packages.claude.digest.as_str()] {
            assert!(
                text.contains(expected),
                "{expected} is not installed: {listed}"
            );
        }
    }

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

    /// The operator path: this build's CLI checks, then upgrades in one step
    /// without accepting runtime loss. The check and the upgrade both judge the
    /// sessions adoptable and plan the head's packages. A built-in start
    /// installs them and binds every profile. A package start installs the
    /// head's packages beside the previous ones: a newer head version repins
    /// the profiles to the head digest, and the same digest keeps the pins.
    fn upgrade(&self) {
        let (live, stopped, head_version) = (&self.id, &self.stopped_id, &self.head_version);
        let planned = self.planned_packages();
        let stopped_code = self.stopped_code();

        let check = self.head.json(&["service", "check", "--json"]);
        eprintln!("service check: {check}");
        assert_preflight_adoptable(
            &check,
            live,
            stopped,
            stopped_code,
            head_version,
            "service check",
        );
        assert_runtime_step(
            &check["preflight"]["runtimes"],
            &planned,
            &self.profile_steps(false),
            "service check",
        );
        self.assert_repins_named(&check["preflight"]["runtimes"], "service check");
        self.assert_strict_repins("service check");

        let from = self.run.head_dir.display().to_string();
        let report = self
            .head
            .json(&["service", "upgrade", "--from", &from, "--json"]);
        eprintln!("service upgrade: {report}");
        assert_upgrade_report(&report, &self.previous_version, &self.head_version);
        // After an apply the upgrade's preflight is the judgement against the
        // installed packages, which carries no plan; `service check` above
        // carries the plan and `runtimes` below the applied step.
        assert_preflight_adoptable(
            &report,
            live,
            stopped,
            stopped_code,
            head_version,
            "service upgrade",
        );
        let applied: Vec<(&str, &str, &str)> = planned
            .iter()
            .map(|(runtime, digest, status)| {
                let applied = if *status == "present" {
                    "present"
                } else {
                    "installed"
                };
                (*runtime, *digest, applied)
            })
            .collect();
        assert_runtime_step(
            &report["runtimes"],
            &applied,
            &self.profile_steps(true),
            "service upgrade",
        );
        self.assert_repins_named(&report["runtimes"], "service upgrade");
        self.assert_strict_repins("service upgrade");
        assert_claude_hooks_reinstalled(&report);
        self.assert_profile_files_pinned();
        // The upgrade does not relaunch a session, so the stopped session keeps
        // its pin until its resume.
        assert_store_pin(
            &self.run,
            stopped,
            CLAUDE_RUNTIME_ID,
            start_pin(&self.start, CLAUDE_PACKAGE_ID),
            &SESSION_COPY,
            "after the upgrade: stopped session",
        );
    }

    /// Each profile file names the package it is based on, at the digest
    /// [`Self::profile_digest`] gives, with the top-level `package` and `digest`
    /// keys.
    fn assert_profile_files_pinned(&self) {
        for (name, package_id, digest) in self.pinned_profiles() {
            let path = self
                .run
                .config_dir()
                .join("agents")
                .join(format!("{name}.toml"));
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let document: toml::Table = toml::from_str(&text)
                .unwrap_or_else(|error| panic!("{}: not TOML: {error}", path.display()));
            assert_eq!(
                document.get("package").and_then(toml::Value::as_str),
                Some(package_id),
                "{} does not pin {package_id}",
                path.display()
            );
            assert_eq!(
                document.get("digest").and_then(toml::Value::as_str),
                Some(digest),
                "{} does not pin {digest}",
                path.display()
            );
        }
    }

    /// The same worker still owns the live session: adoption, not a relaunch.
    /// The store keeps the pin the session started with, because only a
    /// relaunch records a new one.
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
        assert_store_pin(
            &self.run,
            &self.id,
            CLAUDE_RUNTIME_ID,
            start_pin(&self.start, CLAUDE_PACKAGE_ID),
            &REQUIRED_COPIES,
            "after the adoption",
        );
        after
    }

    /// The previous release's daemon resizes the live session, so its worker
    /// holds resize sequences from that daemon.
    fn resize_live_on_previous(&self) {
        for cols in RESIZES_BEFORE_UPGRADE_COLS {
            assert_eq!(
                resize(&self.previous, &self.id, cols, ROWS_BEFORE_UPGRADE),
                (u64::from(cols), u64::from(ROWS_BEFORE_UPGRADE)),
                "the previous release applies its own resize"
            );
        }
    }

    /// The head's daemon numbers its resizes from the start, below the
    /// sequences the previous release's worker kept; the adopted worker must
    /// still apply them, and the session must report the applied size.
    fn assert_resize_applied_by_adopted_worker(&self) {
        let (cols, rows) = RESIZE_AFTER_UPGRADE;
        let requested = (u64::from(cols), u64::from(rows));
        assert_eq!(
            resize(&self.head, &self.id, cols, rows),
            requested,
            "the adopted previous-release worker must apply the head daemon's resize"
        );
        let inspected = inspect(&self.head, &self.id).expect("inspect the resized session");
        assert_eq!(
            (inspected["cols"].as_u64(), inspected["rows"].as_u64()),
            (Some(requested.0), Some(requested.1)),
            "the session reports the applied size: {inspected}"
        );
    }

    /// This build serves the package `digest` of `package_id`, healthy,
    /// enabled, and referenced by a session.
    fn assert_package_served(&self, package_id: &str, digest: &str) {
        let inspected = self.head.json(&[
            "plugin", "inspect", package_id, "--digest", digest, "--json",
        ]);
        let info = &inspected["package"];
        assert_eq!(info["digest"], digest, "{inspected}");
        assert_eq!(info["enabled"], true, "{inspected}");
        assert_eq!(info["referenced"], true, "{inspected}");
        assert!(
            info.get("fault").is_none_or(Value::is_null),
            "the installed package has a fault: {inspected}"
        );
    }

    /// `screen` and `input` reach the adopted worker through the new daemon,
    /// and the previous release's hooks, running in the previous release's
    /// worker, still reach the new daemon. The hook event at the end is sent
    /// through the hooks the upgrade reinstalled.
    fn assert_session_works(&self) {
        let (head, id) = (&self.head, &self.id);
        wait_screen(head, id, "fake-agent ready");
        input(head, id, "echo:input-after-upgrade");
        wait_screen(head, id, "fake-agent ack input-after-upgrade");
        wait_native_id(head, id, NATIVE_BEFORE);
        input(head, id, &format!("report:{NATIVE_AFTER}"));
        wait_active_agent_id(head, id, NATIVE_AFTER);
        wait_native_id(head, id, NATIVE_AFTER);
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

    /// Resumes session `id` with a plain `session resume`, which must succeed:
    /// the profile of the session names the package the upgrade left it on, and
    /// a profile bound to the adopting package resumes without accepting a
    /// change. The relaunch runs in a new generation of this build's worker,
    /// with the native reference the session stopped with, and is pinned to the
    /// package its profile names.
    fn resume(&self, id: &str, native: &str, stopped: &Runtime) -> Runtime {
        let launched = self.run.launches().len();
        let notified = notification_count(&self.head, id, EVENT_AFTER_RESUME);
        self.head.json(&["session", "resume", id, "--json"]);
        self.assert_relaunched(id, native, stopped, launched, notified)
    }

    /// Checks a relaunch of session `id` after `launched` launches and
    /// `notified` resume notifications: a new generation of this build's
    /// worker, launched with `--resume <native>`, whose hooks deliver a new
    /// resume notification and whose store pin is the package its profile names.
    fn assert_relaunched(
        &self,
        id: &str,
        native: &str,
        stopped: &Runtime,
        launched: usize,
        notified: usize,
    ) -> Runtime {
        let head = &self.head;
        let resumed = wait_live(head, id);
        assert!(
            resumed.generation > stopped.generation
                && resumed.worker_instance_id != stopped.worker_instance_id,
            "resume must start a new runtime generation: {stopped:?} -> {resumed:?}"
        );
        wait_screen(head, id, &format!("args=[--resume {native}]"));
        let logged = self.run.wait_launches(launched + 1);
        assert_eq!(
            logged.len(),
            launched + 1,
            "one launch per runtime: {logged:?}"
        );
        assert!(
            logged[launched].contains(&format!("--resume {native}")),
            "resume launched without the native reference: {logged:?}"
        );
        let status = poll_until("the resumed worker runs from this build", || {
            let status = service_status(head);
            (worker_version(&status, id).as_deref() == Some(self.head_version.as_str()))
                .then_some(status)
        });
        assert_eq!(status["active_version"], self.head_version.as_str());
        wait_native_id(head, id, native);
        wait_new_notification(head, id, EVENT_AFTER_RESUME, notified);
        assert_store_pin(
            &self.run,
            id,
            CLAUDE_RUNTIME_ID,
            Pin::Package {
                package_id: CLAUDE_PACKAGE_ID,
                digest: self.profile_digest(CLAUDE_PACKAGE_ID),
            },
            &REQUIRED_COPIES,
            "after the resume",
        );
        resumed
    }

    /// New sessions of the Claude and Codex profiles launch through the
    /// packages their profiles name, and the Codex session takes input.
    fn assert_new_sessions_launch_through_packages(&self) {
        let (head, cwd) = (&self.head, self.run.home.display().to_string());
        let claude = new_session(head, PROFILE, "upgrade-new-claude", &cwd);
        wait_live(head, &claude);
        wait_screen(head, &claude, "fake-agent ready");
        wait_native_id(head, &claude, NATIVE_BEFORE);
        assert_store_pin(
            &self.run,
            &claude,
            CLAUDE_RUNTIME_ID,
            Pin::Package {
                package_id: CLAUDE_PACKAGE_ID,
                digest: self.profile_digest(CLAUDE_PACKAGE_ID),
            },
            &REQUIRED_COPIES,
            "new Claude session",
        );
        self.assert_package_served(CLAUDE_PACKAGE_ID, self.profile_digest(CLAUDE_PACKAGE_ID));

        let codex = new_session(head, CODEX_PROFILE, "upgrade-new-codex", &cwd);
        wait_live(head, &codex);
        wait_screen(head, &codex, "fake-codex ready");
        input(head, &codex, "echo:input-to-codex");
        wait_screen(head, &codex, "fake-codex ack input-to-codex");
        assert_store_pin(
            &self.run,
            &codex,
            CODEX_RUNTIME_ID,
            Pin::Package {
                package_id: CODEX_PACKAGE_ID,
                digest: self.profile_digest(CODEX_PACKAGE_ID),
            },
            &SESSION_COPY,
            "new Codex session",
        );
        self.assert_package_served(CODEX_PACKAGE_ID, self.profile_digest(CODEX_PACKAGE_ID));
    }

    /// The published client's request deadline bounds a silent-reader create.
    fn assert_previous_client_creates_with_input(&self) {
        let cwd = self.run.home.display().to_string();
        let created = self.previous.json(&[
            "session",
            "new",
            "--agent",
            SILENT_PROFILE,
            "--cwd",
            &cwd,
            "--input",
            "echo:input-from-previous-create",
            "--json",
        ]);
        let id = created
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("previous client did not receive a session id: {created}"));
        wait_screen(
            &self.previous,
            id,
            "silent-agent ack echo:input-from-previous-create",
        );
        let removed = self.head.json(&["session", "rm", id, "--json"]);
        assert_eq!(
            removed["removed"], true,
            "compat session was not removed: {removed}"
        );
    }
}

/// Checks the hooks the upgrade reinstalled: the Claude integration is listed
/// and every integration is installed, except a Hermes home without the
/// pohunek marker, which is a `gap` because that integration is installed only
/// by hand.
fn assert_claude_hooks_reinstalled(report: &Value) {
    let integrations = report["integrations"]
        .as_array()
        .unwrap_or_else(|| panic!("the upgrade carries no integrations: {report}"));
    assert!(
        integrations
            .iter()
            .any(|item| item["integration"] == "claude"),
        "the upgrade did not reinstall the Claude hooks: {report}"
    );
    for item in integrations {
        let unmarked_hermes_home = item["integration"] == "hermes"
            && item["status"] == "gap"
            && item["detail"]
                .as_str()
                .is_some_and(|detail| detail.starts_with("no pohunek marker in this home;"));
        assert!(
            item["status"] == "installed" || unmarked_hermes_home,
            "an integration was not reinstalled: {item} in {report}"
        );
    }
}

#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1, a disposable account with a systemd user manager, and the release binaries; run scripts/upgrade-test"]
fn previous_release_session_survives_the_upgrade_to_head() {
    let run = Run::from_env();
    let previous = Cli::new("previous", &run.previous_dir, &run.artifacts);
    let head = Cli::new("head", &run.head_dir, &run.artifacts);
    let (upgrade, before) = start_on_previous(run, previous, head);
    upgrade.capture_previous_goldens();
    upgrade.point_claude_agent_at_newer_range();
    upgrade.resize_live_on_previous();
    upgrade.upgrade();
    upgrade.assert_downgrade_guard();
    let after = upgrade.assert_adopted(&before.live, before.daemon);
    upgrade.assert_resize_applied_by_adopted_worker();
    upgrade.assert_session_works();
    upgrade.assert_head_links_a_package_of_its_format();
    upgrade.assert_previous_client(&after);

    // The stopped session resumes with a plain resume onto the package its
    // profile names; a second stop and plain resume succeed.
    let relaunched = upgrade.resume(&upgrade.stopped_id, NATIVE_STOPPED, &before.stopped);
    upgrade.assert_package_served(CLAUDE_PACKAGE_ID, upgrade.profile_digest(CLAUDE_PACKAGE_ID));
    let stopped = stop_session(&upgrade.head, &upgrade.stopped_id);
    assert_eq!(
        stopped, relaunched,
        "the relaunched session stops as it runs"
    );
    upgrade.resume(&upgrade.stopped_id, NATIVE_STOPPED, &stopped);

    // The adopted live session stops and resumes through the package too.
    let stopped = stop_session(&upgrade.head, &upgrade.id);
    upgrade.resume(&upgrade.id, NATIVE_AFTER, &stopped);

    upgrade.assert_new_sessions_launch_through_packages();
    upgrade.assert_previous_client_creates_with_input();
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
