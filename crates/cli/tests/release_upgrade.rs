//! A session started on the previous release survives `pohunek service
//! upgrade` to this build, driven through the operator path with the real
//! published binaries of the previous release and the real binaries of this
//! build.
//!
//! The previous release installs the service, installs the official Claude
//! runtime package from its own release archive under that archive's signed
//! catalog, pins both host profiles to the package with `plugin profile
//! migrate`, installs its managed Claude hooks, and starts a session whose
//! agent is a fake `claude` that runs those hook scripts. The session is
//! launched from the package, so its recorded launch binding names the package
//! digest. This build's CLI then checks and upgrades the service: the
//! preflight must judge the session adoptable, and the store must keep the
//! same package pin after the adoption and after `resume`, which relaunches
//! from that digest. The assertions follow the live session across the daemon
//! swap, through the previous release's own client, and through `stop` and
//! `resume`.
//!
//! The fake agent is a script whose interpreter line names a copy of the shell
//! called `fake-claude-sh`. The worker designates the agent process by an
//! executable name containing the provider name, and a script's executable is
//! its interpreter, so the copy's name is what qualifies the process. The
//! profile launches the script directly, so the package's version probe runs
//! it too, and it answers `--version` with the lowest release the installed
//! package's descriptor accepts.
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
//! - `POHUNEK_PREVIOUS_RUNTIME_DIR`: the `runtime/` directory of the previous
//!   release's daemon archive, holding `packages/claude.tar.zst` and
//!   `runtime-catalog.json` (v0.34.2 is the first release that ships both);
//! - `POHUNEK_HEAD_BIN_DIR`: the same three binaries of this build, with a
//!   version that differs from the previous release;
//! - `POHUNEK_UPGRADE_ARTIFACTS_DIR`: writable directory receiving the
//!   previous release's store, journals, `service.toml`, and `--json` outputs.
//!
//! The previous release's archive may be of an older package format than this
//! build reads (the 0.35 move to `ruzstd` 0.9.1), so the test takes its digest
//! from the raw bytes and its descriptor from a plain zstd and tar decode, and
//! never hands it to the strict canonical reader. Installed packages keep
//! working across such an upgrade, because the registry keeps verified roots,
//! never archive bytes. After the upgrade the head links a small package
//! directory, so its daemon builds and installs an archive of the head's own
//! format, while the session stays pinned to the previous package.
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
use pohunek_test_support::wait::poll_until;
use serde_json::Value;

/// Source of the fake agent the session launches.
const FAKE_AGENT: &str = include_str!("support/release_upgrade_agent.sh");

/// Silent reader used to verify the previous client's request deadline.
const SILENT_AGENT: &str = include_str!("support/release_upgrade_silent_agent.sh");

/// Shell the fake agents' interpreter is a copy of.
const SHELL: &str = "/bin/sh";

/// File name of that copy. It must contain `claude`: the worker accepts a
/// launch process for the claude hooks only when its executable name does.
const AGENT_INTERPRETER: &str = "fake-claude-sh";

/// Longest interpreter line (`#!` and the path) the kernel reads. Linux reads
/// the first `BINPRM_BUF_SIZE` (256) bytes of a script, newline included, and
/// refuses to execute a script whose interpreter path does not fit.
const MAX_INTERPRETER_LINE_BYTES: usize = 255;

/// Package id of the official Claude runtime package.
const CLAUDE_PACKAGE_ID: &str = "pohunek.runtime.claude";

/// Runtime id the Claude package serves; the host profiles' base.
const CLAUDE_RUNTIME_ID: &str = "claude";

/// The Claude package archive, relative to the release archive's `runtime/`.
const CLAUDE_PACKAGE_ARCHIVE: &str = "packages/claude.tar.zst";

/// The signed runtime catalog, relative to the release archive's `runtime/`.
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
/// Host profile name of the fake agent.
const PROFILE: &str = "fake-claude";

/// Host profile used only for the previous client's initial-input create.
const SILENT_PROFILE: &str = "silent-claude";

/// Native id the fresh launch reports through the previous release's hook.
const NATIVE_BEFORE: &str = "native-before-upgrade";

/// Id reported after the upgrade. The new daemon persists this later report
/// as the current native conversation for a verified launch process.
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
    previous_runtime_dir: PathBuf,
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
            previous_runtime_dir: path("POHUNEK_PREVIOUS_RUNTIME_DIR"),
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

/// The official Claude package the previous release ships, read from its
/// archive so the test knows what the install must report.
struct ClaudePackage {
    /// The package archive inside the previous release's `runtime/`.
    archive: PathBuf,
    /// The signed catalog that authorizes it.
    catalog: PathBuf,
    /// Digest of the archive bytes, `sha256:<hex>`.
    digest: String,
    /// What `claude --version` prints for the lowest release the package's
    /// version probe accepts.
    version_line: String,
}

impl ClaudePackage {
    /// Reads the package archive of the previous release with the strict
    /// archive reader and takes the version line from its own descriptor, the
    /// one both releases probe the fake agent with.
    fn read(runtime_dir: &Path) -> Self {
        let archive = runtime_dir.join(CLAUDE_PACKAGE_ARCHIVE);
        let catalog = runtime_dir.join(RUNTIME_CATALOG);
        assert!(
            catalog.is_file(),
            "the previous release ships no {}",
            catalog.display()
        );
        let bytes = fs::read(&archive)
            .unwrap_or_else(|error| panic!("read {}: {error}", archive.display()));
        let digest = archive_digest(&bytes).expect("the archive digest");
        let contents = tar_member(&zstd_decode(&bytes), PACKAGE_DESCRIPTOR)
            .unwrap_or_else(|| panic!("{} has no {PACKAGE_DESCRIPTOR}", archive.display()));
        let text = std::str::from_utf8(&contents).expect("the descriptor is UTF-8");
        let descriptor: toml::Table = toml::from_str(text).expect("the descriptor is TOML");
        assert_eq!(
            descriptor.get("id").and_then(toml::Value::as_str),
            Some(CLAUDE_PACKAGE_ID),
            "{} is not the Claude package",
            archive.display()
        );
        let probe = descriptor
            .get("runtime")
            .and_then(|runtime| runtime.get("version_probe"))
            .and_then(toml::Value::as_table)
            .expect("the Claude descriptor declares a version probe");
        let field = |name: &str| {
            probe
                .get(name)
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("the Claude version probe has no `{name}`"))
        };
        assert_eq!(
            field("parser"),
            VERSION_LINE_PARSER,
            "the fake agents answer only a line-template version probe"
        );
        let line = field("line");
        assert_eq!(
            line.matches(VERSION_PLACEHOLDER).count(),
            1,
            "the version line template names the release once: {line}"
        );
        Self {
            archive,
            catalog,
            digest: digest.as_str().to_owned(),
            version_line: line.replace(VERSION_PLACEHOLDER, field("min")),
        }
    }
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
/// `interpreter`, defining the `FAKE_CLAUDE_VERSION_LINE` its `--version`
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
    let text = format!("{interpreter_line}\nFAKE_CLAUDE_VERSION_LINE='{version_line}'\n{body}");
    fs::write(path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

/// Writes the fake agents, their host profiles, and the Claude config
/// directory the managed hooks are installed into.
///
/// The profiles carry no package pin: the previous release adds it with
/// `plugin profile migrate` once the package is installed.
fn prepare_agent(run: &Run, package: &ClaudePackage) {
    let private = |path: &Path| {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    };
    private(&run.agent_dir());
    private(&run.home.join(".claude"));
    let interpreter = run.agent_dir().join(AGENT_INTERPRETER);
    let shell = fs::canonicalize(SHELL).expect("resolve the shell");
    fs::copy(&shell, &interpreter).expect("copy the shell as the agent interpreter");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700))
        .expect("chmod the agent interpreter");
    let script = run.agent_dir().join("fake-claude.sh");
    write_agent_script(&script, FAKE_AGENT, &interpreter, &package.version_line);
    let silent_script = run.agent_dir().join("silent-claude.sh");
    write_agent_script(
        &silent_script,
        SILENT_AGENT,
        &interpreter,
        &package.version_line,
    );

    let agents = run.config_dir().join("agents");
    private(&agents);
    // Input is pasted as plain text: the fake agent reads lines with `read`
    // and would see the bracketed-paste escapes otherwise.
    let profile = format!(
        "base = \"{CLAUDE_RUNTIME_ID}\"\nprogram = \"{}\"\n[env]\nFAKE_AGENT_LOG = \"{}\"\n\
         FAKE_AGENT_HOOKS = \"{}\"\nFAKE_AGENT_NATIVE_ID = \"{NATIVE_BEFORE}\"\n\
         [input_rules]\nbracketed_paste = false\n",
        script.display(),
        run.agent_log().display(),
        run.claude_hooks().display(),
    );
    let path = agents.join(format!("{PROFILE}.toml"));
    fs::write(&path, profile).expect("write the host profile");
    let silent_profile = format!(
        "base = \"{CLAUDE_RUNTIME_ID}\"\nprogram = \"{}\"\n[input_rules]\nbracketed_paste = false\n",
        silent_script.display(),
    );
    let silent_path = agents.join(format!("{SILENT_PROFILE}.toml"));
    fs::write(&silent_path, silent_profile).expect("write the silent reader profile");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod the profile");
    fs::set_permissions(&silent_path, fs::Permissions::from_mode(0o600))
        .expect("chmod the silent profile");
}

/// Installs the official Claude package through the previous release's CLI,
/// authorized by the signed catalog of its own archive, and checks that the
/// installed version is that archive, official, enabled and selected.
fn install_package(previous: &Cli, package: &ClaudePackage) {
    let archive = package.archive.display().to_string();
    let catalog = package.catalog.display().to_string();
    let installed = previous.json(&[
        "plugin",
        "install",
        &archive,
        "--catalog",
        &catalog,
        "--yes",
        "--json",
    ]);
    let info = &installed["package"];
    assert_eq!(info["digest"], package.digest.as_str(), "{installed}");
    assert_eq!(info["package"]["id"], CLAUDE_PACKAGE_ID, "{installed}");
    assert_eq!(info["origin"], "official", "{installed}");
    assert_eq!(info["enabled"], true, "{installed}");
    assert_eq!(info["selected"], true, "{installed}");
    assert_eq!(info["runtime_id"], CLAUDE_RUNTIME_ID, "{installed}");
}

/// Pins both host profiles to the installed package through the previous
/// release's CLI.
fn migrate_profiles(previous: &Cli, package: &ClaudePackage) {
    for profile in [PROFILE, SILENT_PROFILE] {
        let migrated = previous.json(&["plugin", "profile", "migrate", profile, "--yes", "--json"]);
        assert_eq!(migrated["status"], "bound", "{profile}: {migrated}");
        assert_eq!(migrated["base"], CLAUDE_RUNTIME_ID, "{profile}: {migrated}");
        assert_eq!(
            migrated["package"]["digest"],
            package.digest.as_str(),
            "{profile}: {migrated}"
        );
    }
}

/// Where the metadata store keeps a copy of a session's launch binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingCopy {
    /// `recovery.launch_binding` of the session's `kind: "session"` record.
    SessionRecovery,
    /// `launch_binding` of the session's `kind: "resume"` record.
    ResumeRecord,
}

/// The copies every checked phase requires. The session record carries the
/// recovery binding the session was launched with. The resume record exists
/// while the session is live and has reported its native reference: the daemon
/// upserts it after the report and removes it when the session ends
/// (`persist_resume_binding` in `crates/daemon/src/session/resume.rs`, the same
/// in v0.34.2). Each check runs after the live session reported its native id.
const REQUIRED_COPIES: [BindingCopy; 2] = [BindingCopy::SessionRecovery, BindingCopy::ResumeRecord];

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

/// Whether `copies` holds each of [`REQUIRED_COPIES`] exactly once and every
/// copy pins the Claude package with `digest`; the error names the first
/// violation.
fn check_pins(copies: &[(BindingCopy, Option<Value>)], digest: &str) -> Result<(), String> {
    for required in REQUIRED_COPIES {
        let count = copies.iter().filter(|(copy, _)| *copy == required).count();
        if count != 1 {
            return Err(format!("{count} {required:?} copies, expected one"));
        }
    }
    for (copy, binding) in copies {
        let Some(binding) = binding else {
            return Err(format!("the {copy:?} copy has no launch binding"));
        };
        let provenance = &binding["provenance"];
        if !(binding["runtime_id"] == CLAUDE_RUNTIME_ID
            && provenance["kind"] == "package"
            && provenance["package"]["id"] == CLAUDE_PACKAGE_ID
            && provenance["package_digest"] == digest)
        {
            return Err(format!(
                "the {copy:?} copy is not pinned to {CLAUDE_PACKAGE_ID} {digest}: {binding}"
            ));
        }
    }
    Ok(())
}

/// The store pins session `id` to the Claude package with `digest` in each of
/// [`REQUIRED_COPIES`], and nothing else.
///
/// The resume record is written after the native report the caller waited
/// for, so the check waits until every required copy is present, then judges
/// the copies once: a copy that is present but unpinned or pinned elsewhere
/// fails at once.
fn assert_store_pin(run: &Run, id: &str, digest: &str, phase: &str) {
    let path = run.data_dir().join("metadata.jsonl");
    let read = || {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{phase}: read {}: {error}", path.display()));
        parse_launch_bindings(&text, id)
            .unwrap_or_else(|error| panic!("{phase}: {}: {error}", path.display()))
    };
    let copies = poll_until(&format!("{phase}: stored launch bindings of {id}"), || {
        let copies = read();
        REQUIRED_COPIES
            .iter()
            .all(|required| copies.iter().any(|(copy, _)| copy == required))
            .then_some(copies)
    });
    if let Err(violation) = check_pins(&copies, digest) {
        panic!("{phase}: {id}: {violation}: {copies:?}");
    }
}

/// The adoption preflight in a `service check` or `service upgrade` payload
/// was run by this build's daemon, judges session `id` adoptable, and puts no
/// live session at risk. The daemon version rules out the previous daemon's
/// rollback verdict, which the same field carries for a rollback.
fn assert_preflight_adoptable(payload: &Value, id: &str, head_version: &str, operation: &str) {
    assert_eq!(
        payload.pointer("/preflight/daemon_version"),
        Some(&Value::from(head_version)),
        "{operation}: the preflight was not run by this build's daemon: {payload}"
    );
    let sessions = payload
        .pointer("/preflight/sessions")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{operation} carries no preflight sessions: {payload}"));
    assert!(
        sessions
            .iter()
            .any(|session| session["session_id"] == id && session["verdict"] == "adoptable"),
        "{operation}: the preflight does not judge {id} adoptable: {payload}"
    );
    assert!(
        sessions
            .iter()
            .all(|session| session["verdict"] == "adoptable"),
        "{operation}: the preflight puts a session at risk: {payload}"
    );
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
    /// Digest of the Claude package the session is pinned to.
    digest: String,
}

/// The previous release installs itself, the official Claude package and its
/// managed hooks, pins the host profiles to the package, then runs a session
/// launched from the package whose agent reports a native id and a
/// notification through the hooks.
fn start_on_previous(run: Run, previous: Cli, head: Cli) -> (Upgrade, Runtime, u64) {
    let (previous_version, head_version) = (previous.version(), head.version());
    assert_ne!(
        previous_version, head_version,
        "an upgrade to the same version is a no-op and proves nothing"
    );
    eprintln!("upgrade {previous_version} -> {head_version}");
    let package = ClaudePackage::read(&run.previous_runtime_dir);
    eprintln!(
        "claude package {} answers `{}`",
        package.digest, package.version_line
    );
    prepare_agent(&run, &package);
    let from = run.previous_dir.display().to_string();
    previous.json(&["service", "install", "--from", &from, "--json"]);
    install_package(&previous, &package);
    migrate_profiles(&previous, &package);
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
    assert_store_pin(&run, &id, &package.digest, "before the upgrade");
    let daemon = daemon_pid(&service_status(&previous));
    let upgrade = Upgrade {
        run,
        previous,
        head,
        previous_version,
        head_version,
        id,
        digest: package.digest,
    };
    (upgrade, before, daemon)
}

impl Upgrade {
    /// The head links a package directory, so the head daemon builds an
    /// archive of the head's package format and installs it. Across a format
    /// change that is the first archive of the new format on this host; it
    /// installs next to the previous release's package, which the session
    /// stays pinned to, and both are listed afterwards. The probe serves an
    /// unreserved runtime, which a linked package may claim.
    fn assert_head_links_a_package_of_its_format(&self) {
        let dir = self.run.home.join(PROBE_DIRECTORY);
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
        let info = &linked["package"];
        assert_eq!(info["package"]["id"], PROBE_PACKAGE_ID, "{linked}");
        let digest = info["digest"].as_str().expect("the linked digest");
        assert_ne!(digest, self.digest, "{linked}");
        let listed = self.head.json(&["plugin", "list", "--json"]);
        let text = listed.to_string();
        for expected in [digest, self.digest.as_str()] {
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

    /// The operator path: this build's CLI checks, then upgrades. Neither
    /// accepts runtime loss, so a refusal by the adoption preflight fails here,
    /// and both preflights must judge the package-pinned session adoptable.
    fn upgrade(&self) {
        let check = self.head.json(&["service", "check", "--json"]);
        eprintln!("service check: {check}");
        assert_preflight_adoptable(&check, &self.id, &self.head_version, "service check");
        let from = self.run.head_dir.display().to_string();
        let report = self
            .head
            .json(&["service", "upgrade", "--from", &from, "--json"]);
        eprintln!("service upgrade: {report}");
        assert_upgrade_report(&report, &self.previous_version, &self.head_version);
        assert_preflight_adoptable(&report, &self.id, &self.head_version, "service upgrade");
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
        assert_store_pin(&self.run, &self.id, &self.digest, "after the adoption");
        self.assert_package_served();
        after
    }

    /// This build serves the package version the previous release installed,
    /// healthy, enabled, and held by the session's pin.
    fn assert_package_served(&self) {
        let inspected = self.head.json(&[
            "plugin",
            "inspect",
            CLAUDE_PACKAGE_ID,
            "--digest",
            &self.digest,
            "--json",
        ]);
        let info = &inspected["package"];
        assert_eq!(info["digest"], self.digest.as_str(), "{inspected}");
        assert_eq!(info["enabled"], true, "{inspected}");
        assert_eq!(info["referenced"], true, "{inspected}");
        assert!(
            info.get("fault").is_none_or(Value::is_null),
            "the installed package has a fault: {inspected}"
        );
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
        wait_native_id(head, id, NATIVE_AFTER);
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
        wait_screen(head, id, &format!("args=[--resume {NATIVE_AFTER}]"));
        let launches = self.run.wait_launches(2);
        assert_eq!(launches.len(), 2, "one launch per runtime: {launches:?}");
        assert!(
            launches[1].contains(&format!("--resume {NATIVE_AFTER}")),
            "resume launched without the native reference: {launches:?}"
        );
        let status = poll_until("the resumed worker runs from this build", || {
            let status = service_status(head);
            (worker_version(&status, id).as_deref() == Some(self.head_version.as_str()))
                .then_some(status)
        });
        assert_eq!(status["active_version"], self.head_version.as_str());
        wait_native_id(head, id, NATIVE_AFTER);
        wait_notification(head, id, EVENT_AFTER_RESUME);
        assert_store_pin(&self.run, id, &self.digest, "after the resume");

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
    upgrade.assert_head_links_a_package_of_its_format();
    upgrade.assert_previous_client(&after);
    upgrade.assert_stop_and_resume(&after);
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
