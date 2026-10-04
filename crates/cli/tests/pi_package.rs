//! The official Pi runtime package (`runtime-packages/pi`).
//!
//! Pure tests always run: they build the package directory, parse it through
//! the same path `plugin install` uses, and check it against the compatibility
//! lock, the fixture screens captured from a real Pi, and the session-file
//! layout the existence check searches. The real-Pi test drives an actual
//! `pi` binary through the installed package; see its documentation for the
//! opt-in.

// Rust guideline compliant 2026-10-04

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use std::time::{Duration, Instant};

use package::directory::build_directory_archive;
use package::{read_archive, Limits, PackageDigest};
use pohunek_daemon::agent::host::{definition_from_archive, LaunchProgram, RuntimeDefinition};
use pohunek_daemon::agent::{NativeReferenceStrategy, ReferenceCheckFailure, SessionRef};
use pohunek_daemon::detect::{DetectionConfig, Detector, DetectorConfig};
use pohunek_daemon::procwatch::{ProcessFact, StartIdentity};
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::workspace_root;
use protocol::{AgentActivity, BindingProvenance, StateSource};
use serde_json::Value;

#[path = "support/model_stub.rs"]
mod model_stub;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use model_stub::ModelStub;
use plugin_harness::{path_str, Harness};

/// Directory of the package source, relative to the workspace root.
const PACKAGE_DIR: &str = "runtime-packages/pi";

/// Compatibility lock of the package, relative to the workspace root.
const LOCK_PATH: &str = "compat/pi/compatibility-lock.json";

/// Package id the descriptor declares.
const PACKAGE_ID: &str = "pohunek.runtime.pi";

/// Profile the real-Pi test launches through.
const PROFILE: &str = "pi-test";

fn package_dir() -> PathBuf {
    workspace_root().join(PACKAGE_DIR)
}

/// The canonical archive of the package directory and its digest.
fn built_archive() -> (Vec<u8>, PackageDigest) {
    let bytes = build_directory_archive(&package_dir(), &Limits::DEFAULT)
        .expect("the package directory builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

/// The definition `plugin install` derives from the package directory.
fn installed_definition() -> RuntimeDefinition {
    let (bytes, digest) = built_archive();
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    definition_from_archive(archive.entries(), &digest)
        .expect("the daemon accepts the package descriptor")
}

fn read_lock() -> Value {
    let text = fs::read_to_string(workspace_root().join(LOCK_PATH)).expect("read the lock");
    serde_json::from_str(&text).expect("the lock is JSON")
}

#[test]
fn the_package_directory_builds_reproducibly_with_only_its_two_files() {
    let (first, digest) = built_archive();
    let (second, again) = built_archive();
    assert_eq!(first, second, "two builds produce the same bytes");
    assert_eq!(digest, again);
    let archive = read_archive(&first, &Limits::DEFAULT).expect("the archive reads");
    let paths: Vec<&str> = archive
        .entries()
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(
        paths,
        ["detect.toml", "runtime.toml"],
        "the archive holds only descriptor and manifest; evidence lives in compat/pi"
    );
}

#[test]
fn the_descriptor_declares_the_verified_pi_launch_contract() {
    let definition = installed_definition();
    assert_eq!(definition.runtime_id().as_str(), "pi");
    assert_eq!(definition.display_name(), "Pi");
    assert_eq!(definition.program(), &LaunchProgram::Fixed("pi".to_owned()));
    assert!(definition.default_args().is_empty());
    assert!(
        definition.prompt_arg(),
        "the prompt is a positional argument"
    );
    match &definition.binding().provenance {
        BindingProvenance::Package { package, .. } => {
            assert_eq!(package.id.as_str(), PACKAGE_ID);
            assert_eq!(package.version.as_str(), "1.0.0");
        }
        other @ BindingProvenance::Builtin { .. } => {
            panic!("a package-served runtime, got {other:?}")
        }
    }

    let rules = definition.input_rules();
    assert!(rules.bracketed_paste, "Pi enables bracketed paste");
    assert_eq!(rules.submit_delay, Duration::from_millis(150));

    assert_eq!(
        definition.native_reference_strategy(),
        NativeReferenceStrategy::Assigned
    );
    let native = definition.native().expect("native recovery is declared");
    let reference = SessionRef::id("0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee").expect("reference");
    assert_eq!(
        native.assigned().expect("assigned").launch_argv(&reference),
        ["--session-id", reference.value()]
    );
    assert_eq!(
        native.resume_argv(&reference).expect("resume"),
        ["--session", reference.value()]
    );
    assert_eq!(
        native.fork_argv(&reference).expect("fork"),
        ["--fork", reference.value()]
    );
}

#[test]
fn the_supported_range_equals_the_compatibility_lock() {
    let definition = installed_definition();
    let policy = definition
        .version_probe_policy()
        .expect("the descriptor declares a version probe");
    assert_eq!(
        definition
            .version_probe_parser()
            .map(pohunek_daemon::agent::host::HandlerId::as_str),
        Some("semver-v1")
    );
    assert_eq!(policy.args(), ["--version"]);

    let lock = read_lock();
    assert_eq!(lock["package"], PACKAGE_ID);
    assert_eq!(lock["runtime"], definition.runtime_id().as_str());
    assert_eq!(lock["supported"]["min"], policy.min().to_string());
    assert_eq!(lock["supported"]["below"], policy.below().to_string());

    let release = locked_release();
    let release = pohunek_daemon::agent::host::ProbeVersion::parse(&release)
        .expect("the locked release is MAJOR.MINOR.PATCH");
    assert!(
        policy.supports(release),
        "the locked release lies in the supported range"
    );
    assert!(
        !policy.supports(policy.below()),
        "the first release above the range is unsupported"
    );
}

/// Lays out `<root>/sessions/<project>/<timestamp>_<reference>.jsonl` the way
/// Pi 1.0.2 writes it and returns the root.
fn pi_agent_dir(project: &str, file: &str) -> tempfile::TempDir {
    let root = pohunek_test_support::tempdir().expect("tempdir");
    let directory = root.path().join("sessions").join(project);
    fs::create_dir_all(&directory).expect("create the project directory");
    fs::write(directory.join(file), b"{}\n").expect("write the session file");
    root
}

fn existence_check(
    definition: &RuntimeDefinition,
    reference: &str,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<(), ReferenceCheckFailure> {
    let reference = SessionRef::id(reference).expect("reference");
    definition
        .native()
        .and_then(|native| native.assigned())
        .expect("assigned reference")
        .existence()
        .verify(&reference, env)
}

/// The file name Pi 1.0.2 produced for `--session-id <id>` in a PTY
/// (`<ISO time with : and . as ->_<id>.jsonl`) below `--<cwd with / as ->--`.
const REAL_REFERENCE: &str = "0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";
const REAL_FILE_NAME: &str = "2026-10-04T19-18-34-412Z_0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee.jsonl";
const REAL_PROJECT_DIR: &str = "--tmp-project--";

#[test]
fn the_existence_check_finds_a_conversation_by_the_real_pi_file_layout() {
    let definition = installed_definition();
    let root = pi_agent_dir(REAL_PROJECT_DIR, REAL_FILE_NAME);
    let dir = root.path().to_owned();
    let env =
        move |name: &str| (name == "PI_CODING_AGENT_DIR").then(|| dir.clone().into_os_string());

    existence_check(&definition, REAL_REFERENCE, &env).expect("the session file is found");
    assert_eq!(
        existence_check(&definition, "0197aaaa-bbbb-7ccc-8ddd-ffffffffffff", &env),
        Err(ReferenceCheckFailure::Missing),
        "another reference names no conversation"
    );
    assert_eq!(
        existence_check(&definition, "eeeeeeeeeeee", &env),
        Err(ReferenceCheckFailure::Missing),
        "a partial id is not a reference: the separator before the id must match"
    );
}

#[test]
fn the_existence_check_resolves_the_agent_directory_like_pi() {
    let definition = installed_definition();
    let home = pohunek_test_support::tempdir().expect("tempdir");
    let agent = home
        .path()
        .join(".pi/agent/sessions")
        .join(REAL_PROJECT_DIR);
    fs::create_dir_all(&agent).expect("create the default agent directory");
    fs::write(agent.join(REAL_FILE_NAME), b"{}\n").expect("write the session file");

    // PI_CODING_AGENT_DIR unset: the default `~/.pi/agent` below HOME.
    let home_dir = home.path().to_owned();
    let from_home = move |name: &str| (name == "HOME").then(|| home_dir.clone().into_os_string());
    existence_check(&definition, REAL_REFERENCE, &from_home).expect("found below ~/.pi/agent");

    // PI_CODING_AGENT_DIR set elsewhere: that directory wins and is empty.
    let empty = pohunek_test_support::tempdir().expect("tempdir");
    let (home_dir, empty_dir) = (home.path().to_owned(), empty.path().to_owned());
    let redirected = move |name: &str| match name {
        "HOME" => Some(home_dir.clone().into_os_string()),
        "PI_CODING_AGENT_DIR" => Some(empty_dir.clone().into_os_string()),
        _ => None,
    };
    assert_eq!(
        existence_check(&definition, REAL_REFERENCE, &redirected),
        Err(ReferenceCheckFailure::Missing)
    );

    // Neither variable: nothing to search.
    assert_eq!(
        existence_check(&definition, REAL_REFERENCE, &|_name: &str| None),
        Err(ReferenceCheckFailure::RootUnavailable)
    );
}

#[test]
fn the_existence_check_refuses_a_symlinked_project_directory() {
    let definition = installed_definition();
    let root = pohunek_test_support::tempdir().expect("tempdir");
    let elsewhere = pohunek_test_support::tempdir().expect("tempdir");
    fs::write(elsewhere.path().join(REAL_FILE_NAME), b"{}\n").expect("write the file");
    fs::create_dir_all(root.path().join("sessions")).expect("create sessions");
    std::os::unix::fs::symlink(
        elsewhere.path(),
        root.path().join("sessions").join(REAL_PROJECT_DIR),
    )
    .expect("symlink");
    let dir = root.path().to_owned();
    let env =
        move |name: &str| (name == "PI_CODING_AGENT_DIR").then(|| dir.clone().into_os_string());
    assert!(
        existence_check(&definition, REAL_REFERENCE, &env).is_err(),
        "a conversation behind a symlink is never accepted"
    );
}

/// A screen the way the terminal tracker receives it: cleared, then rows.
fn screen_bytes(path: &str) -> Vec<u8> {
    let text = fs::read_to_string(workspace_root().join("compat/pi/screens").join(path))
        .expect("read the fixture screen");
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(text.trim_end_matches('\n').replace('\n', "\r\n").as_bytes());
    bytes
}

fn detector(definition: &RuntimeDefinition, now: Instant) -> Detector {
    Detector::new(
        24,
        100,
        now,
        DetectorConfig {
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some((**definition.manifest()).clone()),
        },
    )
}

#[test]
fn the_manifest_classifies_screens_captured_from_a_real_pi() {
    let definition = installed_definition();
    let started = Instant::now();
    let mut detector = detector(&definition, started);
    let idle = |activity| {
        vec![pohunek_daemon::detect::ActivityTransition {
            activity,
            source: StateSource::Screen,
        }]
    };

    assert_eq!(
        detector.feed(started, &screen_bytes("idle.txt")),
        idle(AgentActivity::Idle),
        "the plain editor rules mean idle"
    );
    assert_eq!(
        detector.feed(started, &screen_bytes("working.txt")),
        idle(AgentActivity::Working),
        "the spinner rule with `Working` means working"
    );
    assert_eq!(
        detector.feed(started, &screen_bytes("idle_after_turn.txt")),
        idle(AgentActivity::Idle),
        "after the reply the plain rules return"
    );
}

#[test]
fn a_screen_without_pi_chrome_is_never_classified() {
    let definition = installed_definition();
    let started = Instant::now();
    let mut detector = detector(&definition, started);
    for screen in [
        &b"\x1b[2J\x1b[Huser@host:~$ ls\r\nCargo.toml  src\r\nuser@host:~$ "[..],
        // A rule of ordinary text-mode output that carries no editor frame.
        &b"\x1b[2J\x1b[H-----\r\nWorking directory clean\r\n"[..],
    ] {
        let transitions = detector.feed(started, screen);
        assert!(
            transitions
                .iter()
                .all(|transition| transition.source != StateSource::Screen),
            "only the byte-activity fallback may speak for a foreign screen: {transitions:?}"
        );
    }
}

fn process(comm: &str, cmdline: &[&str]) -> ProcessFact {
    ProcessFact {
        pid: 4242,
        pgid: 4242,
        ppid: 1,
        start_identity: StartIdentity::new(1),
        comm: comm.to_owned(),
        cmdline: cmdline
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
    }
}

#[test]
fn the_process_matchers_accept_only_the_observed_pi_process_forms() {
    let definition = installed_definition();
    let matchers = definition
        .manifest()
        .process_matchers()
        .expect("the manifest declares process matchers");

    // Steady state: Pi retitles itself, so both the kernel name and the argv
    // are `pi`, the argv padded with empty entries.
    let padded: Vec<&str> = std::iter::once("pi")
        .chain(std::iter::repeat_n("", 20))
        .collect();
    assert!(matchers.matches(&process("pi", &padded)));
    // The first moments: node running the installed bundle.
    for cli in [
        "/usr/lib/node_modules/pi/packages/coding-agent/dist/bundle/cli.js",
        "/opt/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js",
    ] {
        assert!(
            matchers.matches(&process("node", &["node", cli, "--session-id", "x"])),
            "{cli}"
        );
    }

    for (comm, cmdline) in [
        ("node", vec!["node", "/work/app/dist/bundle/cli.js"]),
        ("node", vec!["node", "/work/server.js", "coding-agent"]),
        ("python3", vec!["python3", "pi.py"]),
        ("pipe", vec!["pipe"]),
        ("zsh", vec!["zsh", "-c", "pi"]),
    ] {
        assert!(
            !matchers.matches(&process(comm, &cmdline)),
            "{comm} {cmdline:?}"
        );
    }
}

/// Opt-in variable of the real-Pi test.
const E2E_VARIABLE: &str = "POHUNEK_PI_E2E";

/// Optional variable naming an archive built by `cargo xtask package build`.
///
/// The real-Pi test installs that file and requires its digest to equal the
/// digest of the in-process build of the same directory.
const ARCHIVE_VARIABLE: &str = "POHUNEK_PI_PACKAGE_ARCHIVE";

/// Placeholder API key of the loopback provider; the stub never checks it.
const STUB_KEY: &str = "stub";

/// Name of the provider the Pi configuration points at the stub.
const STUB_PROVIDER: &str = "stub";

/// The prompt the test submits.
const PROMPT: &str = "say hi";

/// Looks `name` up on `PATH` the way the daemon resolves a program.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Fails the test, naming the opt-in, unless a real `pi` is available.
///
/// The test is `#[ignore]`d so a run without Pi skips it with that reason; a
/// run that selects it (`--ignored`, as the `pi package` CI job does) must have
/// the opt-in variable and a `pi` on `PATH`, and fails loudly otherwise.
fn require_real_pi() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "the real-Pi test needs {E2E_VARIABLE}=1 and the pinned `pi` on PATH"
    );
    assert!(
        find_on_path("pi").is_some(),
        "{E2E_VARIABLE}=1 but no `pi` executable is on PATH"
    );
}

/// The release the compatibility lock pins.
fn locked_release() -> String {
    let lock = read_lock();
    lock["upstream"]["release"]
        .as_str()
        .expect("lock release")
        .to_owned()
}

/// A real daemon with a hermetic Pi configuration pointing at a loopback
/// model stub.
struct Fixture {
    harness: Harness,
    stub: ModelStub,
    digest: PackageDigest,
    archive: PathBuf,
    sessions_dir: PathBuf,
}

impl Fixture {
    async fn start() -> Self {
        let harness = Harness::start().await;
        let stub = ModelStub::start();
        let agent_dir = harness.env.home().join(".pi/agent");
        fs::create_dir_all(&agent_dir).expect("create the Pi agent directory");
        fs::write(
            agent_dir.join("models.json"),
            serde_json::json!({
                "providers": {
                    STUB_PROVIDER: {
                        "baseUrl": stub.base_url(),
                        "api": "openai-completions",
                        "apiKey": STUB_KEY,
                        "models": [{ "id": model_stub::MODEL_ID }],
                    }
                }
            })
            .to_string(),
        )
        .expect("write models.json");
        fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "defaultProvider": STUB_PROVIDER,
                "defaultModel": model_stub::MODEL_ID,
            })
            .to_string(),
        )
        .expect("write settings.json");

        let (bytes, digest) = built_archive();
        let archive = if let Some(path) = std::env::var_os(ARCHIVE_VARIABLE) {
            let path = PathBuf::from(path);
            let supplied = fs::read(&path).expect("read the supplied archive");
            let supplied_digest = read_archive(&supplied, &Limits::DEFAULT)
                .expect("the supplied archive reads")
                .digest()
                .clone();
            assert_eq!(
                supplied_digest, digest,
                "`cargo xtask package build` and the in-process build of {PACKAGE_DIR} differ"
            );
            path
        } else {
            let path = harness.env.root().join("pi.tar.zst");
            fs::write(&path, bytes).expect("write the archive");
            path
        };
        Self {
            sessions_dir: agent_dir.join("sessions"),
            harness,
            stub,
            digest,
            archive,
        }
    }

    /// Installs the package with explicit-digest trust, as an owner would.
    async fn install(&self) {
        let (code, installed) = self
            .harness
            .json(&[
                "plugin",
                "install",
                path_str(&self.archive),
                "--sha256",
                self.digest.as_str(),
                "--yes",
            ])
            .await;
        assert_eq!(code, 0, "{installed}");
        assert_eq!(installed["ok"]["status"], "installed", "{installed}");
        assert_eq!(
            installed["ok"]["package"]["origin"], "explicit_digest",
            "a local install is never official: {installed}"
        );
    }

    /// Writes the profile the sessions launch through: it pins the installed
    /// digest and keeps Pi off the network.
    fn write_profile(&self) {
        self.harness.profile(
            PROFILE,
            &format!(
                "base = \"pi\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n\n[env]\nPI_OFFLINE = \"1\"\nPI_SKIP_VERSION_CHECK = \"1\"\nPI_TELEMETRY = \"0\"\n",
                self.digest
            ),
        );
    }

    async fn new_session(&self, name: &str) -> Value {
        let cwd = self.harness.env.cwd().to_path_buf();
        let (code, launched) = self
            .harness
            .json(&[
                "session",
                "new",
                "--agent",
                PROFILE,
                "--cwd",
                path_str(&cwd),
                "--name",
                name,
            ])
            .await;
        assert_eq!(code, 0, "{launched}");
        launched["ok"].clone()
    }

    /// Waits until the session's detected activity is `activity` and returns
    /// the session record that matched.
    async fn wait_activity(&self, id: &str, activity: &str) -> Value {
        wait_until(&format!("activity {activity} of {id}"), || async {
            let (code, waited) = self
                .harness
                .json(&[
                    "session",
                    "wait",
                    id,
                    "--activity",
                    activity,
                    "--timeout-ms",
                    "8000",
                ])
                .await;
            assert_eq!(code, 0, "{waited}");
            (waited["ok"]["reason"] == "activity_matched").then(|| waited["ok"]["session"].clone())
        })
        .await
    }

    /// The visible terminal lines of the session.
    async fn screen(&self, id: &str) -> Vec<String> {
        let (code, screen) = self.harness.json(&["session", "screen", id]).await;
        assert_eq!(code, 0, "{screen}");
        screen["ok"]["visible_lines"]
            .as_array()
            .expect("visible lines")
            .iter()
            .map(|line| line.as_str().expect("line").trim_end().to_owned())
            .collect()
    }

    /// Waits until the visible screen holds a line containing `needle`.
    async fn wait_screen_line(&self, id: &str, needle: &str) -> Vec<String> {
        wait_until(&format!("a screen line containing {needle:?}"), || async {
            let lines = self.screen(id).await;
            lines
                .iter()
                .any(|line| line.contains(needle))
                .then_some(lines)
        })
        .await
    }

    /// Stops the session and waits until its runtime is no longer live.
    async fn stop(&self, id: &str) {
        let (code, stopped) = self.harness.json(&["session", "stop", id]).await;
        assert_eq!(code, 0, "{stopped}");
        wait_until(&format!("session {id} to stop"), || async {
            let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
            assert_eq!(code, 0, "{inspected}");
            (inspected["ok"]["runtime"]["state"] != "live").then_some(())
        })
        .await;
    }

    /// Every `*.jsonl` file below the Pi sessions directory.
    fn session_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Ok(projects) = fs::read_dir(&self.sessions_dir) else {
            return files;
        };
        for project in projects {
            let project = project.expect("project entry").path();
            for entry in fs::read_dir(&project).expect("read a project directory") {
                let path = entry.expect("session entry").path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    /// The session file whose name ends with `_<reference>.jsonl`.
    fn session_file(&self, reference: &str) -> Option<PathBuf> {
        let suffix = format!("_{reference}.jsonl");
        self.session_files().into_iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
    }
}

/// The first JSON line of a Pi session file.
fn session_header(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read the session file");
    let line = text.lines().next().expect("a header line");
    serde_json::from_str(line).expect("the header is JSON")
}

/// The probe ran the real `pi --version` and the daemon accepts the release.
async fn check_inventory(fixture: &Fixture) {
    let (code, host) = fixture.harness.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{host}");
    let pi = host["ok"]["runtimes"]
        .as_array()
        .expect("runtimes")
        .iter()
        .find(|runtime| runtime["agent"] == "pi")
        .expect("the package serves the pi runtime")
        .clone();
    assert_eq!(pi["available"], true, "{pi}");
    assert_eq!(pi["supported"], true, "{pi}");
    assert_eq!(
        pi["version"],
        locked_release(),
        "the real Pi is the locked release"
    );
}

/// Launches through the installed package, detects idle, submits a prompt,
/// detects working while the stub holds the reply, and returns the session id,
/// its assigned reference and the session file Pi wrote.
async fn launch_and_converse(fixture: &Fixture) -> (String, String, PathBuf) {
    let harness = &fixture.harness;
    let launched = fixture.new_session("e2e").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    assert_eq!(launched["agent_base"], "pi");
    assert_eq!(launched["capabilities"]["resume"], true);
    assert_eq!(launched["capabilities"]["fork"], true);
    let reference = launched["native_session_id"]
        .as_str()
        .expect("the daemon assigned a native reference")
        .to_owned();

    // Detection: the idle editor rule of the real screen.
    let idle = fixture.wait_activity(&id, "idle").await;
    assert_eq!(idle["state_source"], "screen", "{idle}");

    // Input: bracketed paste then a separate submit; the stub holds the reply
    // so the working rule is on screen.
    let (code, sent) = harness.json(&["session", "input", &id, PROMPT]).await;
    assert_eq!(code, 0, "{sent}");
    let working = fixture.wait_activity(&id, "working").await;
    assert_eq!(working["state_source"], "screen", "{working}");
    let lines = fixture.screen(&id).await;
    assert!(
        lines.iter().any(|line| line.contains("Working")),
        "the working rule is on screen: {lines:#?}"
    );
    assert_eq!(fixture.stub.started(), 1, "one model request");
    fixture.stub.open_gate();
    fixture.wait_activity(&id, "idle").await;
    fixture
        .wait_screen_line(
            &id,
            &format!("{}{}", model_stub::FIRST_CHUNK, model_stub::SECOND_CHUNK),
        )
        .await;

    // The session file Pi wrote carries the reference the daemon assigned.
    let file = fixture
        .session_file(&reference)
        .expect("Pi wrote <timestamp>_<reference>.jsonl below its sessions directory");
    let header = session_header(&file);
    assert_eq!(header["type"], "session");
    assert_eq!(header["id"], reference.as_str());
    assert_eq!(
        header["cwd"],
        harness.env.cwd().to_str().expect("utf-8 cwd"),
        "Pi names the project directory after the launch cwd"
    );

    // The pin keeps the package installed while the session exists.
    let (code, refused) = harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_referenced");
    (id, reference, file)
}

/// Stops the session and resumes it: the conversation comes back from Pi's
/// session file without a new model request.
async fn check_resume(fixture: &Fixture, id: &str) {
    fixture.stop(id).await;
    let (code, resumed) = fixture.harness.json(&["session", "resume", id]).await;
    assert_eq!(code, 0, "{resumed}");
    fixture.wait_activity(id, "idle").await;
    let restored = fixture.wait_screen_line(id, PROMPT).await;
    assert!(
        restored
            .iter()
            .any(|line| line.contains(model_stub::FIRST_CHUNK)),
        "the earlier turn is on screen after resume: {restored:#?}"
    );
    assert_eq!(fixture.stub.started(), 1, "resume makes no model request");
}

/// Forks the live session: a new session with the conversation in its own Pi
/// session file, which names the source as its parent and is not resumable.
async fn check_fork(fixture: &Fixture, id: &str, source_file: &Path) {
    let (code, forked) = fixture
        .harness
        .json(&["session", "fork", id, "--name", "e2e-fork"])
        .await;
    assert_eq!(code, 0, "{forked}");
    let fork_id = forked["ok"]["id"].as_str().expect("fork id").to_owned();
    assert_ne!(fork_id, id);
    fixture.wait_activity(&fork_id, "idle").await;
    fixture.wait_screen_line(&fork_id, PROMPT).await;
    let files = fixture.session_files();
    assert_eq!(
        files.len(),
        2,
        "the fork wrote its own session file: {files:?}"
    );
    let fork_file = files
        .iter()
        .find(|path| path.as_path() != source_file)
        .expect("the fork's file");
    assert_eq!(
        session_header(fork_file)["parentSession"],
        source_file.to_str().expect("utf-8 path"),
        "the fork names the source conversation as its parent"
    );

    // The fork's reference is unknown to the daemon, so it fails closed.
    fixture.stop(&fork_id).await;
    let (code, refused) = fixture.harness.json(&["session", "resume", &fork_id]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "not_resumable", "{refused}");
}

/// Drives a real `pi` through the installed package: launch, detect, input,
/// stop, resume, fork and the existence check against Pi's own session files.
///
/// Needs a `pi` on `PATH` and `POHUNEK_PI_E2E=1`; the model is a loopback
/// stub, so no provider, credential or network is involved.
#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn a_real_pi_session_launches_is_detected_resumes_and_forks() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    fixture.write_profile();

    check_inventory(&fixture).await;
    let (id, _reference, file) = launch_and_converse(&fixture).await;
    check_resume(&fixture, &id).await;
    check_fork(&fixture, &id, &file).await;

    // The existence check follows Pi's files: without the session file the
    // daemon refuses before launching anything.
    fixture.stop(&id).await;
    fs::remove_file(&file).expect("remove the session file");
    let (code, refused) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "agent_native_reference_missing",
        "{refused}"
    );

    fixture.harness.stop().await;
}

/// A Pi session that never reached its first model reply has no session file,
/// so recovery refuses instead of relaunching an empty conversation.
#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn a_real_pi_session_without_a_turn_has_nothing_to_resume() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    fixture.write_profile();

    let launched = fixture.new_session("unprompted").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    let reference = launched["native_session_id"]
        .as_str()
        .expect("native reference")
        .to_owned();
    fixture.wait_activity(&id, "idle").await;
    assert!(
        fixture.session_file(&reference).is_none(),
        "Pi writes no session file before the first reply"
    );

    fixture.stop(&id).await;
    let (code, refused) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "agent_native_reference_missing",
        "{refused}"
    );
    fixture.harness.stop().await;
}
