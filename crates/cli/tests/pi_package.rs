//! The official Pi runtime package (`runtime-packages/pi`).
//!
//! Pure tests always run: they build the package directory, parse it through
//! the same path `plugin install` uses, and check it against the compatibility
//! lock, the fixture screens captured from a real Pi, and the session-file
//! layouts the existence check searches. The real-Pi tests drive an actual
//! `pi` binary through the installed package; see their documentation for the
//! opt-in.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::time::{Duration, Instant};

use package::directory::build_directory_archive;
use package::{read_archive, Limits, PackageDigest};
use pohunek_daemon::agent::host::{
    definition_from_archive, InputReadiness, LaunchProgram, RuntimeDefinition,
};
use pohunek_daemon::agent::{
    LaunchView, NativeReferenceStrategy, ReferenceCheckFailure, ReferenceExistence, RefusalPhase,
    SessionRef,
};
use pohunek_daemon::detect::{DetectionConfig, Detector, DetectorConfig};
use pohunek_daemon::procwatch::{ProcessFact, StartIdentity};
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::workspace_root;
use protocol::{AgentActivity, BindingProvenance, StateSource};
use serde_json::Value;

#[path = "support/capture.rs"]
mod capture;
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

/// Profile the real-Pi tests launch through by default.
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
            assert_eq!(package.version.as_str(), "1.0.3");
        }
        other @ BindingProvenance::Builtin { .. } => {
            panic!("a package-served runtime, got {other:?}")
        }
    }

    let rules = definition.input_rules();
    assert!(rules.bracketed_paste, "Pi enables bracketed paste");
    assert_eq!(rules.submit_delay, Duration::from_millis(150));
    assert_eq!(
        definition.input_readiness(),
        InputReadiness::ScreenIdle,
        "the idle editor frame, not the bracketed-paste enable, shows readiness"
    );

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
    // A fork gets a reference of its own: `pi --fork <source> --session-id
    // <fork>` creates the forked conversation under that id.
    let fork = native.fork_argv(&reference).expect("fork");
    let fork_reference = fork.reference.expect("the fork is assigned a reference");
    assert_ne!(fork_reference, reference);
    assert_eq!(
        fork.argv,
        [
            "--fork",
            reference.value(),
            "--session-id",
            fork_reference.value()
        ]
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
    let next_major = policy
        .min()
        .to_string()
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        .expect("the minimum's major release")
        + 1;
    assert_eq!(
        policy.below().to_string(),
        format!("{next_major}.0.0"),
        "the range runs from the verified minimum to the next major release"
    );
    assert!(
        policy.supports(
            pohunek_daemon::agent::host::ProbeVersion::parse("1.1.0").expect("a release")
        ),
        "a later 1.x release lies in the supported range"
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

/// The existence check Pi's package declares, as recovery and a first launch
/// run it.
fn pi_existence(definition: &RuntimeDefinition) -> ReferenceExistence {
    definition
        .native()
        .and_then(|native| native.assigned())
        .expect("assigned reference")
        .existence()
        .clone()
}

/// A `--session-dir` directory the way Pi 1.0.2 fills it: the session files
/// sit directly in it, as a PTY run with `PI_CODING_AGENT_SESSION_DIR` showed.
fn pi_session_dir(file: &str) -> tempfile::TempDir {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    fs::write(dir.path().join(file), b"{}\n").expect("write the session file");
    dir
}

#[test]
fn the_existence_check_follows_the_session_dir_variable() {
    let definition = installed_definition();
    let agent = pi_agent_dir(REAL_PROJECT_DIR, "unrelated.jsonl");
    let store = pi_session_dir(REAL_FILE_NAME);
    let (agent_dir, store_dir) = (agent.path().to_owned(), store.path().to_owned());
    let env = move |name: &str| match name {
        "PI_CODING_AGENT_DIR" => Some(agent_dir.clone().into_os_string()),
        "PI_CODING_AGENT_SESSION_DIR" => Some(store_dir.clone().into_os_string()),
        _ => None,
    };
    existence_check(&definition, REAL_REFERENCE, &env)
        .expect("the session file is found in the variable's directory");

    // Pi keeps the files directly in that directory and never looks below it.
    let nested = pohunek_test_support::tempdir().expect("tempdir");
    fs::create_dir(nested.path().join(REAL_PROJECT_DIR)).expect("create a subdirectory");
    fs::write(
        nested.path().join(REAL_PROJECT_DIR).join(REAL_FILE_NAME),
        b"{}\n",
    )
    .expect("write a nested session file");
    let nested_dir = nested.path().to_owned();
    let below = move |name: &str| {
        (name == "PI_CODING_AGENT_SESSION_DIR").then(|| nested_dir.clone().into_os_string())
    };
    assert_eq!(
        existence_check(&definition, REAL_REFERENCE, &below),
        Err(ReferenceCheckFailure::Missing)
    );

    // An empty variable is unset, as for Pi; a relative one is refused.
    let agent_dir = agent.path().to_owned();
    let empty = move |name: &str| match name {
        "PI_CODING_AGENT_DIR" => Some(agent_dir.clone().into_os_string()),
        "PI_CODING_AGENT_SESSION_DIR" => Some(std::ffi::OsString::new()),
        _ => None,
    };
    assert_eq!(
        existence_check(&definition, REAL_REFERENCE, &empty),
        Err(ReferenceCheckFailure::Missing),
        "the agent directory's store holds no such session"
    );
    let relative = |name: &str| {
        (name == "PI_CODING_AGENT_SESSION_DIR").then(|| std::ffi::OsString::from("pi-sessions"))
    };
    assert_eq!(
        existence_check(&definition, REAL_REFERENCE, &relative),
        Err(ReferenceCheckFailure::StoreEnvRelative)
    );
}

/// Runs the relocation check of the Pi package against a launch of `args` in
/// `cwd` whose environment is `vars`.
fn relocation(
    definition: &RuntimeDefinition,
    vars: &[(&str, &Path)],
    args: &[&str],
    cwd: &Path,
) -> Result<(), ReferenceCheckFailure> {
    let vars: Vec<(String, std::ffi::OsString)> = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.as_os_str().to_owned()))
        .collect();
    let env = move |name: &str| {
        vars.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    pi_existence(definition).check_relocation(&LaunchView {
        env: &env,
        args: &args,
        cwd,
    })
}

#[test]
fn a_launch_whose_pi_keeps_its_sessions_elsewhere_is_refused() {
    let definition = installed_definition();
    let agent = pohunek_test_support::tempdir().expect("agent dir");
    let project = pohunek_test_support::tempdir().expect("project");
    let store = pohunek_test_support::tempdir().expect("session dir");
    let agent_only = [("PI_CODING_AGENT_DIR", agent.path())];
    let with_store = [
        ("PI_CODING_AGENT_DIR", agent.path()),
        ("PI_CODING_AGENT_SESSION_DIR", store.path()),
    ];
    let cwd = project.path();
    assert_eq!(relocation(&definition, &agent_only, &[], cwd), Ok(()));

    // The flag wins over everything in Pi, in both spellings Pi's parser and
    // a careless profile could produce.
    for args in [&["--session-dir", "/x"][..], &["--session-dir=/x"][..]] {
        assert_eq!(
            relocation(&definition, &with_store, args, cwd),
            Err(ReferenceCheckFailure::RelocatedByArgument),
            "{args:?}"
        );
    }
    assert_eq!(
        relocation(&definition, &agent_only, &["--session-directory"], cwd),
        Ok(()),
        "another option is not the flag"
    );

    // `sessionDir` in the agent directory's or the project's settings.
    let global = agent.path().join("settings.json");
    let local = project.path().join(".pi/settings.json");
    fs::create_dir_all(project.path().join(".pi")).expect("create the project settings dir");
    for (settings, file) in [(&global, "agent"), (&local, "project")] {
        fs::write(settings, "\u{feff}{\"sessionDir\": \"~/pi-sessions\"}")
            .expect("write the settings");
        assert_eq!(
            relocation(&definition, &agent_only, &[], cwd),
            Err(ReferenceCheckFailure::RelocatedBySetting),
            "{file} settings"
        );
        // The variable wins over the settings in Pi, and the check follows it.
        assert_eq!(relocation(&definition, &with_store, &[], cwd), Ok(()));
        fs::remove_file(settings).expect("remove the settings");
    }

    // Values Pi treats as unset, and a file it cannot parse.
    for (contents, expected) in [
        (r#"{"sessionDir": ""}"#, Ok(())),
        (r#"{"sessionDir": null, "theme": "dark"}"#, Ok(())),
        ("", Ok(())),
        ("{not json", Err(ReferenceCheckFailure::SettingsUnreadable)),
    ] {
        fs::write(&global, contents).expect("write the settings");
        assert_eq!(
            relocation(&definition, &agent_only, &[], cwd),
            expected,
            "{contents:?}"
        );
    }
}

#[test]
fn a_refused_launch_names_the_session_dir_variable() {
    let definition = installed_definition();
    let existence = pi_existence(&definition);
    for failure in [
        ReferenceCheckFailure::RelocatedByArgument,
        ReferenceCheckFailure::RelocatedBySetting,
        ReferenceCheckFailure::SettingsUnreadable,
        ReferenceCheckFailure::StoreEnvRelative,
    ] {
        let error = existence.refusal(failure, RefusalPhase::Launch);
        assert_eq!(error.code, "agent_native_store_relocated", "{failure}");
        assert!(
            error
                .recover
                .as_deref()
                .is_some_and(|hint| hint.contains("PI_CODING_AGENT_SESSION_DIR")),
            "{failure}: {error:?}"
        );
    }
    // The flag wins over the variable in Pi, so the hint removes it first.
    let flag = existence.refusal(
        ReferenceCheckFailure::RelocatedByArgument,
        RefusalPhase::Launch,
    );
    assert!(
        flag.recover.as_deref().is_some_and(
            |hint| hint.starts_with("remove --session-dir from the agent profile args")
        ),
        "{flag:?}"
    );
    assert_eq!(
        existence
            .refusal(ReferenceCheckFailure::Missing, RefusalPhase::Launch)
            .code,
        "agent_native_reference_missing"
    );
}

#[test]
fn a_refused_recovery_says_to_undo_the_move_instead() {
    let definition = installed_definition();
    let existence = pi_existence(&definition);
    // The launch passed the refusal, so something changed since: a setting
    // appeared or the variable the launch had is gone. The hint undoes the
    // change; pointing the variable at a new directory would not find the
    // conversation.
    let setting = existence.refusal(
        ReferenceCheckFailure::RelocatedBySetting,
        RefusalPhase::Recovery,
    );
    assert_eq!(setting.code, "agent_native_store_relocated");
    let hint = setting.recover.expect("a recovery hint");
    assert!(hint.contains("remove the setting"), "{hint}");
    assert!(
        hint.contains("restore PI_CODING_AGENT_SESSION_DIR to the absolute path the session was launched with"),
        "{hint}"
    );
    assert!(!hint.contains("added after"), "{hint}");
    let relative = existence.refusal(
        ReferenceCheckFailure::StoreEnvRelative,
        RefusalPhase::Recovery,
    );
    assert!(
        relative
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("--accept-profile-change")),
        "{relative:?}"
    );
}

/// How long the relocation check of a settings path may take; a check that
/// blocks on the path never returns, so any bound far above a file read
/// separates the two.
const SETTINGS_CHECK_BOUND: Duration = Duration::from_secs(10);

#[test]
fn a_settings_path_that_is_not_a_file_refuses_without_blocking() {
    let definition = installed_definition();
    let agent = pohunek_test_support::tempdir().expect("agent dir");
    let project = pohunek_test_support::tempdir().expect("project");
    fs::create_dir(project.path().join(".pi")).expect("create the project settings dir");
    // A repository can carry a FIFO, or a symlink to one, where Pi reads its
    // project settings.
    nix::unistd::mkfifo(
        &project.path().join(".pi/settings.json"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create the FIFO");
    let (agent_dir, cwd) = (agent.path().to_owned(), project.path().to_owned());
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = relocation(
            &definition,
            &[("PI_CODING_AGENT_DIR", agent_dir.as_path())],
            &[],
            &cwd,
        );
        let _ = sender.send(outcome);
    });
    assert_eq!(
        receiver.recv_timeout(SETTINGS_CHECK_BOUND),
        Ok(Err(ReferenceCheckFailure::SettingsUnreadable)),
        "the check refuses the FIFO instead of waiting for a writer"
    );

    // A directory where the file belongs is refused the same way.
    fs::remove_file(project.path().join(".pi/settings.json")).expect("remove the FIFO");
    fs::create_dir(project.path().join(".pi/settings.json")).expect("create a directory");
    let definition = installed_definition();
    assert_eq!(
        relocation(
            &definition,
            &[("PI_CODING_AGENT_DIR", agent.path())],
            &[],
            project.path()
        ),
        Err(ReferenceCheckFailure::SettingsUnreadable)
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
    detector_with_columns(definition, now, 100)
}

fn detector_with_columns(definition: &RuntimeDefinition, now: Instant, columns: u16) -> Detector {
    Detector::new(
        24,
        columns,
        now,
        DetectorConfig {
            input_readiness: pohunek_daemon::agent::host::InputReadiness::default(),
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(Arc::clone(definition.manifest())),
        },
    )
}

/// Fixture screens captured from a real Pi 1.0.2 and the activity each means.
///
/// The `Working` and `Retrying` frames keep the editor rules while a turn runs,
/// so the multi-line drafts (six lines, and a draft scrolled inside the
/// editor's height limit) are the frames whose status indicator sits furthest
/// above the footer.
const CAPTURED_SCREENS: [(&str, AgentActivity); 10] = [
    ("idle.txt", AgentActivity::Idle),
    ("idle_after_turn.txt", AgentActivity::Idle),
    ("idle_draft6.txt", AgentActivity::Idle),
    ("idle_draft_scrolled_up.txt", AgentActivity::Idle),
    ("idle_draft_scrolled_mid.txt", AgentActivity::Idle),
    ("working.txt", AgentActivity::Working),
    ("working_draft6.txt", AgentActivity::Working),
    ("working_draft_scrolled.txt", AgentActivity::Working),
    ("compaction.txt", AgentActivity::Working),
    ("retry.txt", AgentActivity::Working),
];

#[test]
fn every_captured_screen_is_classified_by_screen_evidence() {
    let definition = installed_definition();
    for (screen, expected) in CAPTURED_SCREENS {
        let started = Instant::now();
        let mut detector = detector(&definition, started);
        assert_eq!(
            detector.feed(started, &screen_bytes(screen)),
            vec![pohunek_daemon::detect::ActivityTransition {
                activity: expected,
                source: StateSource::Screen,
            }],
            "{screen}"
        );
    }
}

#[test]
fn a_status_indicator_outranks_the_editor_rules_in_every_working_screen() {
    let definition = installed_definition();
    let started = Instant::now();
    let mut detector = detector(&definition, started);
    let mut now = started;
    // Alternating busy and plain frames: each one moves the activity, so no
    // frame is shadowed by the previous classification.
    for (screen, expected) in [
        ("idle_draft6.txt", AgentActivity::Idle),
        ("working_draft6.txt", AgentActivity::Working),
        ("idle_draft6.txt", AgentActivity::Idle),
        ("compaction.txt", AgentActivity::Working),
        ("idle.txt", AgentActivity::Idle),
        ("retry.txt", AgentActivity::Working),
    ] {
        now += Duration::from_secs(1);
        let transitions = detector.feed(now, &screen_bytes(screen));
        assert_eq!(
            transitions
                .last()
                .map(|transition| (transition.activity, transition.source)),
            Some((expected, StateSource::Screen)),
            "{screen}"
        );
    }
}

/// Rows of a captured screen.
fn screen_rows(path: &str) -> Vec<String> {
    fs::read_to_string(workspace_root().join("compat/pi/screens").join(path))
        .expect("read the fixture screen")
        .trim_end_matches('\n')
        .split('\n')
        .map(str::to_owned)
        .collect()
}

/// The activity the manifest reads from `rows` on a 100-column terminal, when
/// the screen speaks at all.
fn activity_of(definition: &RuntimeDefinition, rows: &[String]) -> Option<AgentActivity> {
    activity_at(definition, rows, 100)
}

/// [`activity_of`] on a terminal `columns` wide.
fn activity_at(
    definition: &RuntimeDefinition,
    rows: &[String],
    columns: u16,
) -> Option<AgentActivity> {
    let started = Instant::now();
    let mut detector = detector_with_columns(definition, started, columns);
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(rows.join("\r\n").as_bytes());
    detector
        .feed(started, &bytes)
        .iter()
        .rfind(|transition| transition.source == StateSource::Screen)
        .map(|transition| transition.activity)
}

/// Row indexes of the upper and lower border of a captured editor frame: the
/// last two rows that start with a rule. Rules above them are transcript.
fn frame_borders(rows: &[String]) -> (usize, usize) {
    let borders: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.starts_with("\u{2500}\u{2500}"))
        .map(|(index, _)| index)
        .collect();
    assert!(borders.len() >= 2, "a captured frame has two borders");
    (borders[borders.len() - 2], borders[borders.len() - 1])
}

/// Text that is not a border wherever it appears: it may look like one in
/// part, but it neither starts with a rule and a spinner nor is a plain rule.
const HOSTILE_TEXT: [&str; 13] = [
    "\u{2500} example",
    "\u{2500}\u{2500} example",
    "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} example",
    "\u{2500}",
    "\u{2500}\u{2500}",
    "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
    "\u{2500}\u{2500}\u{2500}\u{2500} \u{2191} 5 more \u{2500}\u{2500}",
    "\u{2807} Working",
    "Working",
    "Compacting context...",
    "\u{2191} 5 more",
    "x \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} \u{2191} 5 more",
    "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} \u{2807} Working ",
];

/// Lines that start like a busy border at any length. Inside the draft they
/// are read as the frame's status border; above the frame they are transcript.
const STATUS_LOOKALIKES: [&str; 5] = [
    "\u{2500}\u{2500} \u{2807} Working \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
    "\u{2500}\u{2500} \u{283c} Compacting context... (escape to cancel) \u{2500}\u{2500}",
    "\u{2500}\u{2500} \u{2807} Retrying (1/3) in 1s... \u{2500}",
    "\u{2500}\u{2500} \u{2807} Working",
    "\u{2500}\u{2500} \u{2807}",
];

/// Complete forged borders. They are indistinguishable from the frame when
/// they sit inside the draft, but above the frame they are transcript.
fn forged_borders() -> Vec<String> {
    let rule = |count: usize| "\u{2500}".repeat(count);
    vec![
        format!("\u{2500}\u{2500} \u{2807} Working {}", rule(80)),
        format!(
            "\u{2500}\u{2500} \u{283c} Compacting context... (escape to cancel) {}",
            rule(60)
        ),
        format!(
            "\u{2500}\u{2500} \u{2807} Retrying (1/3) in 1s... (escape to cancel) {}",
            rule(50)
        ),
        format!("{} \u{2191} 5 more {}", rule(40), rule(40)),
        rule(100),
        rule(20),
    ]
}

/// Replaces every transcript and draft row of `rows`, one at a time, with
/// hostile text and returns how many substitutions kept the classification.
fn sweep_hostile_rows(
    definition: &RuntimeDefinition,
    name: &str,
    rows: &[String],
    columns: u16,
    expected: AgentActivity,
) -> usize {
    assert_eq!(
        activity_at(definition, rows, columns),
        Some(expected),
        "{name} unmodified"
    );
    let (upper, lower) = frame_borders(rows);
    let mut checked = 0;
    for row in (0..lower).filter(|row| *row != upper) {
        let mut hostile: Vec<String> = HOSTILE_TEXT.iter().map(|text| (*text).to_owned()).collect();
        if row < upper {
            hostile.extend(STATUS_LOOKALIKES.iter().map(|text| (*text).to_owned()));
            hostile.extend(forged_borders());
        }
        for text in hostile {
            let mut changed = rows.to_vec();
            changed[row].clone_from(&text);
            assert_eq!(
                activity_at(definition, &changed, columns),
                Some(expected),
                "{name} row {row} replaced by {text:?}"
            );
            checked += 1;
        }
    }
    checked
}

/// A captured screen of `compat/pi/screens/widths/`: its name, terminal width
/// and the activity it means.
struct WidthFixture {
    name: String,
    columns: u16,
    rows: Vec<String>,
    expected: AgentActivity,
}

/// Every screen rendered by the real Pi 1.0.2 at a given terminal width. The
/// file name is `<state>_w<columns>.txt`; states starting `idle`, `scroll_idle`
/// or `rule_idle` mean idle, every other state is Pi at work.
fn width_fixtures() -> Vec<WidthFixture> {
    let mut names: Vec<String> = fs::read_dir(workspace_root().join("compat/pi/screens/widths"))
        .expect("read the width fixtures")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .into_string()
                .expect("UTF-8 file name")
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let stem = name.strip_suffix(".txt").expect("a .txt fixture");
            let (state, columns) = stem.rsplit_once("_w").expect("a `_w<columns>` suffix");
            let idle = ["idle", "scroll_idle", "rule_idle"]
                .iter()
                .any(|prefix| state.starts_with(prefix));
            WidthFixture {
                columns: columns.parse().expect("a column count"),
                rows: screen_rows(&format!("widths/{name}")),
                expected: if idle {
                    AgentActivity::Idle
                } else {
                    AgentActivity::Working
                },
                name,
            }
        })
        .collect()
}

#[test]
fn every_screen_rendered_at_every_terminal_width_is_classified_without_error() {
    let definition = installed_definition();
    let fixtures = width_fixtures();
    let widths: std::collections::BTreeSet<u16> =
        fixtures.iter().map(|fixture| fixture.columns).collect();
    assert!(
        widths.contains(&20) && widths.contains(&200) && widths.len() >= 15,
        "the fixtures span the supported widths: {widths:?}"
    );
    let busy = fixtures
        .iter()
        .filter(|fixture| fixture.expected == AgentActivity::Working)
        .count();
    assert!(busy >= 60, "busy screens captured: {busy}");
    for fixture in &fixtures {
        assert_eq!(
            activity_at(&definition, &fixture.rows, fixture.columns),
            Some(fixture.expected),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn a_busy_border_shortened_by_the_terminal_width_is_never_idle() {
    let definition = installed_definition();
    // Pi 1.0.2 closes a busy border with `width - status - 4` columns: one
    // column for Retrying at 24 columns and Compacting at 36, two at 50.
    let closing_rule = |fixture: &WidthFixture| {
        let (upper, _) = frame_borders(&fixture.rows);
        fixture.rows[upper]
            .chars()
            .rev()
            .take_while(|ch| *ch == '\u{2500}')
            .count()
    };
    let fixtures = width_fixtures();
    let shortest = fixtures
        .iter()
        .filter(|fixture| fixture.expected == AgentActivity::Working)
        .map(closing_rule)
        .min()
        .expect("busy fixtures exist");
    assert_eq!(shortest, 1, "a captured busy border closes with one rule");
    for fixture in fixtures
        .iter()
        .filter(|fixture| fixture.expected == AgentActivity::Working && closing_rule(fixture) < 10)
    {
        assert_ne!(
            activity_at(&definition, &fixture.rows, fixture.columns),
            Some(AgentActivity::Idle),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn a_column_zero_rule_in_the_transcript_never_stands_in_for_the_upper_border() {
    let definition = installed_definition();
    let fixtures: Vec<WidthFixture> = width_fixtures()
        .into_iter()
        .filter(|fixture| fixture.name.starts_with("rule_"))
        .collect();
    assert!(fixtures.len() >= 9, "rule screens captured");
    for fixture in &fixtures {
        let (upper, _) = frame_borders(&fixture.rows);
        assert!(
            fixture.rows[..upper]
                .iter()
                .any(|row| !row.is_empty() && row.chars().all(|ch| ch == '\u{2500}')),
            "{} has a rule above the editor",
            fixture.name
        );
        assert_eq!(
            activity_at(&definition, &fixture.rows, fixture.columns),
            Some(fixture.expected),
            "{}",
            fixture.name
        );
    }
}

/// Border-like text (rules, scroll markers, status lookalikes) in the
/// transcript or the draft never changes the classification of a captured
/// editor screen. Text that starts like a selector line is the documented
/// exception: see [`a_draft_reads_idle_unless_it_reproduces_a_selector_list_or_a_dialog_hint`].
#[test]
fn hostile_text_in_the_transcript_or_the_draft_never_changes_the_classification() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    for (screen, expected) in CAPTURED_SCREENS {
        checked += sweep_hostile_rows(&definition, screen, &screen_rows(screen), 100, expected);
    }
    // The narrowest terminal, one whose busy borders close with one rule, and
    // the transcript-rule screens at 24 columns.
    for fixture in width_fixtures().iter().filter(|fixture| {
        fixture.columns == 20
            || (fixture.columns == 36 && fixture.name.starts_with("compaction"))
            || (fixture.columns == 24 && fixture.name.starts_with("rule_"))
    }) {
        checked += sweep_hostile_rows(
            &definition,
            &fixture.name,
            &fixture.rows,
            fixture.columns,
            fixture.expected,
        );
    }
    assert!(checked > 3500, "the sweep covers every row: {checked}");
}

/// What an overlay screen of `compat/pi/screens/overlays/` waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
    /// A prompt that waits for an answer: an extension dialog (also raised by
    /// a permission check mid-turn), the project trust or missing session cwd
    /// prompt, or a login step.
    Dialog,
    /// A selector the owner opened (model, session, tree, fork, settings).
    Selector,
    /// A question Pi prints before its interface starts, outside any frame.
    Unframed,
}

/// A screen a real Pi 1.0.2 drew in place of its editor, named
/// `<blocked|selector|unframed>_<what>_w<columns>.txt`.
struct OverlayFixture {
    name: String,
    kind: OverlayKind,
    columns: u16,
    rows: Vec<String>,
}

/// Every overlay screen, captured by driving a real Pi in a terminal of each
/// width with real key presses (model calls answered by a loopback stub).
fn overlay_fixtures() -> Vec<OverlayFixture> {
    let mut names: Vec<String> = fs::read_dir(workspace_root().join("compat/pi/screens/overlays"))
        .expect("read the overlay fixtures")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .into_string()
                .expect("UTF-8 file name")
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let stem = name.strip_suffix(".txt").expect("a .txt fixture");
            let (state, columns) = stem.rsplit_once("_w").expect("a `_w<columns>` suffix");
            let kind = match state.split_once('_').expect("a `<kind>_` prefix").0 {
                "blocked" => OverlayKind::Dialog,
                "selector" => OverlayKind::Selector,
                "unframed" => OverlayKind::Unframed,
                other => panic!("unknown overlay kind {other} in {name}"),
            };
            OverlayFixture {
                kind,
                columns: columns.parse().expect("a column count"),
                rows: screen_rows(&format!("overlays/{name}")),
                name,
            }
        })
        .collect()
}

#[test]
fn no_overlay_reads_idle_at_any_terminal_width() {
    let definition = installed_definition();
    let fixtures = overlay_fixtures();
    let widths: std::collections::BTreeSet<u16> =
        fixtures.iter().map(|fixture| fixture.columns).collect();
    assert!(
        widths.contains(&20) && widths.contains(&200) && widths.len() >= 10,
        "the overlays span the supported widths: {widths:?}"
    );
    assert!(
        fixtures.len() >= 140,
        "overlays captured: {}",
        fixtures.len()
    );
    for fixture in &fixtures {
        assert_ne!(
            activity_at(&definition, &fixture.rows, fixture.columns),
            Some(AgentActivity::Idle),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn a_prompt_that_waits_for_an_answer_reads_blocked_at_every_width() {
    let definition = installed_definition();
    let dialogs: Vec<OverlayFixture> = overlay_fixtures()
        .into_iter()
        .filter(|fixture| fixture.kind == OverlayKind::Dialog)
        .collect();
    assert!(dialogs.len() >= 120, "dialogs captured: {}", dialogs.len());
    let mut clipped = 0_usize;
    for fixture in &dialogs {
        // A prompt taller than the terminal can lose its bottom rows: its
        // lower border, and in some cases its key hint; Pi drew no frame to
        // read then.
        let expected = if frame_closes_the_screen(&fixture.rows) {
            Some(AgentActivity::Blocked)
        } else {
            clipped += 1;
            None
        };
        assert_eq!(
            activity_at(&definition, &fixture.rows, fixture.columns),
            expected,
            "{}",
            fixture.name
        );
    }
    assert!(clipped <= 10, "dialogs without a frame: {clipped}");
}

/// Whether the last plain border of `rows` is followed by at most four
/// lines: the frame (and its footer, if any) ends the screen.
fn frame_closes_the_screen(rows: &[String]) -> bool {
    let mut rows = rows.to_vec();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows.iter()
        .rposition(|row| row.chars().count() >= 20 && row.chars().all(|ch| ch == '\u{2500}'))
        .is_some_and(|lower| rows.len() - lower - 1 <= 4)
}

#[test]
fn a_selector_the_owner_opened_and_an_unframed_question_match_no_rule() {
    let definition = installed_definition();
    for fixture in overlay_fixtures()
        .iter()
        .filter(|fixture| fixture.kind != OverlayKind::Dialog)
    {
        assert_eq!(
            activity_at(&definition, &fixture.rows, fixture.columns),
            None,
            "{}",
            fixture.name
        );
    }
}

#[test]
fn a_dialog_opened_from_the_idle_editor_is_published_as_blocked_and_closes_to_idle() {
    let definition = installed_definition();
    let started = Instant::now();
    let mut detector = detector(&definition, started);
    let mut now = started;
    for (screen, expected) in [
        ("idle_after_turn.txt", AgentActivity::Idle),
        (
            "overlays/blocked_permission_midturn_w100.txt",
            AgentActivity::Blocked,
        ),
        ("idle_after_turn.txt", AgentActivity::Idle),
        (
            "overlays/blocked_project_trust_w100.txt",
            AgentActivity::Blocked,
        ),
        ("working.txt", AgentActivity::Working),
        (
            "overlays/blocked_login_api_key_w100.txt",
            AgentActivity::Blocked,
        ),
    ] {
        now += Duration::from_secs(1);
        let transitions = detector.feed(now, &screen_bytes(screen));
        assert_eq!(
            transitions
                .last()
                .map(|transition| (transition.activity, transition.source)),
            Some((expected, StateSource::Screen)),
            "{screen}"
        );
    }
}

#[test]
fn transcript_text_never_changes_a_dialog_or_a_selector() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    // Screens whose frame shows both borders; a taller overlay has no
    // transcript rows above it.
    let framed = |fixture: &&OverlayFixture| {
        fixture
            .rows
            .iter()
            .filter(|row| row.starts_with("\u{2500}\u{2500}"))
            .count()
            >= 2
    };
    for fixture in overlay_fixtures()
        .iter()
        .filter(|fixture| fixture.columns == 100)
        .filter(framed)
    {
        let expected = activity_at(&definition, &fixture.rows, fixture.columns);
        let (upper, _) = frame_borders(&fixture.rows);
        let mut hostile: Vec<String> = HOSTILE_TEXT.iter().map(|text| (*text).to_owned()).collect();
        hostile.extend(STATUS_LOOKALIKES.iter().map(|text| (*text).to_owned()));
        hostile.extend(forged_borders());
        hostile.extend(
            [
                "> quoted",
                " \u{2192} Yes",
                " \u{2191}\u{2193} navigate  enter select  escape/ctrl+c cancel",
            ]
            .map(str::to_owned),
        );
        for row in 0..upper {
            for text in &hostile {
                let mut changed = fixture.rows.clone();
                changed[row].clone_from(text);
                assert_eq!(
                    activity_at(&definition, &changed, fixture.columns),
                    expected,
                    "{} row {row} replaced by {text:?}",
                    fixture.name
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 2000,
        "the sweep covers every transcript row: {checked}"
    );
}

/// `rows` with the first draft line of its editor frame replaced by `text`.
fn with_draft(rows: &[String], text: &str) -> Vec<String> {
    let (upper, lower) = frame_borders(rows);
    assert!(upper + 1 < lower, "the draft has a line");
    let mut changed = rows.to_vec();
    text.clone_into(&mut changed[upper + 1]);
    changed
}

#[test]
fn a_draft_reads_idle_unless_it_reproduces_a_selector_list_or_a_dialog_hint() {
    let definition = installed_definition();
    let rows = screen_rows("idle_draft6.txt");
    // A quote, a lone cursor or a line that only mentions the markers is
    // draft text: Pi is idle.
    for text in [
        "> quoted reply",
        ">",
        "\u{2192} next",
        " \u{2192} Yes",
        "\u{203a} item",
        "see > below",
        "a \u{2192} b",
        "press enter to submit",
    ] {
        assert_eq!(
            activity_of(&definition, &with_draft(&rows, text)),
            Some(AgentActivity::Idle),
            "{text:?}"
        );
    }
    // A cursor row together with a row indented by two spaces is what a
    // selector list draws; such a draft withholds idle.
    let mut list = with_draft(&rows, "\u{2192} first");
    let (upper, _) = frame_borders(&list);
    list[upper + 2] = "  second".to_owned();
    assert_eq!(activity_of(&definition, &list), None);
    // A draft that reproduces a dialog's key hint reads as the dialog.
    assert_eq!(
        activity_of(
            &definition,
            &with_draft(&rows, " enter submit  escape/ctrl+c cancel")
        ),
        Some(AgentActivity::Blocked)
    );
}

#[test]
fn a_turn_that_ends_with_a_quoted_draft_is_published_idle() {
    let definition = installed_definition();
    let started = Instant::now();
    let mut detector = detector(&definition, started);
    let mut now = started;
    let quote = "> quoted reply";
    for (rows, expected) in [
        (
            with_draft(&screen_rows("idle_draft6.txt"), quote),
            AgentActivity::Idle,
        ),
        (
            with_draft(&screen_rows("working_draft6.txt"), quote),
            AgentActivity::Working,
        ),
        (
            with_draft(&screen_rows("idle_draft6.txt"), quote),
            AgentActivity::Idle,
        ),
    ] {
        now += Duration::from_secs(1);
        let mut bytes = b"\x1b[2J\x1b[H".to_vec();
        bytes.extend_from_slice(rows.join("\r\n").as_bytes());
        let transitions = detector.feed(now, &bytes);
        assert_eq!(
            transitions
                .last()
                .map(|transition| (transition.activity, transition.source)),
            Some((expected, StateSource::Screen)),
            "{rows:#?}"
        );
    }
}

#[test]
fn the_review_counterexamples_keep_their_classification() {
    let definition = installed_definition();
    // A literal status line in place of the first transcript reply.
    let mut rows = screen_rows("idle_draft6.txt");
    rows[3] =
        "\u{2500}\u{2500} \u{2807} Working \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".to_owned();
    assert_eq!(activity_of(&definition, &rows), Some(AgentActivity::Idle));
    // A draft line that begins with a box-drawing dash.
    let mut rows = screen_rows("idle_draft6.txt");
    rows[15] = "\u{2500} example".to_owned();
    assert_eq!(activity_of(&definition, &rows), Some(AgentActivity::Idle));
    // The same draft line while Pi is busy.
    let mut rows = screen_rows("working_draft6.txt");
    rows[15] = "\u{2500} example".to_owned();
    assert_eq!(
        activity_of(&definition, &rows),
        Some(AgentActivity::Working)
    );
}

#[test]
fn an_editor_screen_without_its_footer_or_a_foreign_rule_pair_is_not_idle() {
    let definition = installed_definition();
    let started = Instant::now();
    let rule = "\u{2500}".repeat(40);
    for (name, screen) in [
        // The rules alone, no footer below the lower rule.
        ("no footer", format!("text\r\n{rule}\r\n\r\n{rule}")),
        // A rule pair followed by more output than a footer holds.
        (
            "output below",
            format!("{rule}\r\n\r\n{rule}\r\na\r\nb\r\nc\r\nd\r\ne"),
        ),
        // A lone rule has no editor frame.
        ("one rule", format!("text\r\n{rule}\r\nfooter")),
    ] {
        let mut detector = detector(&definition, started);
        let transitions = detector.feed(started, format!("\x1b[2J\x1b[H{screen}").as_bytes());
        assert!(
            transitions
                .iter()
                .all(|transition| transition.source != StateSource::Screen),
            "{name}: only the byte-activity fallback may speak: {transitions:?}"
        );
    }
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

/// Opt-in variable of the real-Pi tests.
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
        self.write_named_profile(PROFILE, "", "");
    }

    /// Writes profile `name` with extra top-level `args` and `[env]` lines.
    fn write_named_profile(&self, name: &str, args: &str, env: &str) {
        self.harness.profile(
            name,
            &format!(
                "base = \"pi\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n{args}\n[env]\nPI_OFFLINE = \"1\"\nPI_SKIP_VERSION_CHECK = \"1\"\nPI_TELEMETRY = \"0\"\n{env}",
                self.digest
            ),
        );
    }

    async fn new_session(&self, name: &str) -> Value {
        let cwd = self.harness.env.cwd().to_path_buf();
        let (code, launched) = self.launch(PROFILE, name, &cwd).await;
        assert_eq!(code, 0, "{launched}");
        launched["ok"].clone()
    }

    /// Runs `session new` of `profile` in `cwd` and returns its exit code and
    /// JSON document.
    async fn launch(&self, profile: &str, name: &str, cwd: &Path) -> (i32, Value) {
        self.harness
            .json(&[
                "session",
                "new",
                "--agent",
                profile,
                "--cwd",
                path_str(cwd),
                "--name",
                name,
            ])
            .await
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

/// Forks the live session under a reference the daemon assigns, then stops
/// and resumes the fork: Pi names the fork's own session file after that
/// reference, so the existence check finds it and `--session` reopens the
/// forked conversation without a new model request.
async fn check_fork(fixture: &Fixture, id: &str, source_reference: &str, source_file: &Path) {
    let (code, forked) = fixture
        .harness
        .json(&["session", "fork", id, "--name", "e2e-fork"])
        .await;
    assert_eq!(code, 0, "{forked}");
    let fork_id = forked["ok"]["id"].as_str().expect("fork id").to_owned();
    assert_ne!(fork_id, id);
    let fork_reference = forked["ok"]["native_session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the daemon assigned the fork a reference: {forked}"))
        .to_owned();
    assert_ne!(
        fork_reference, source_reference,
        "the fork is a conversation of its own"
    );
    assert_eq!(forked["ok"]["capabilities"]["resume"], true, "{forked}");
    fixture.wait_activity(&fork_id, "idle").await;
    fixture.wait_screen_line(&fork_id, PROMPT).await;

    // Pi writes the fork's file at once, named and identified by the assigned
    // reference, with the source conversation as its parent.
    let files = fixture.session_files();
    assert_eq!(
        files.len(),
        2,
        "the fork wrote its own session file: {files:?}"
    );
    let fork_file = fixture
        .session_file(&fork_reference)
        .expect("Pi wrote <timestamp>_<fork reference>.jsonl");
    assert_ne!(fork_file, source_file);
    let header = session_header(&fork_file);
    assert_eq!(header["id"], fork_reference.as_str());
    assert_eq!(
        header["parentSession"],
        source_file.to_str().expect("utf-8 path"),
        "the fork names the source conversation as its parent"
    );

    // The fork resumes its own conversation from that file.
    fixture.stop(&fork_id).await;
    let (code, resumed) = fixture.harness.json(&["session", "resume", &fork_id]).await;
    assert_eq!(code, 0, "{resumed}");
    fixture.wait_activity(&fork_id, "idle").await;
    let restored = fixture.wait_screen_line(&fork_id, PROMPT).await;
    assert!(
        restored
            .iter()
            .any(|line| line.contains(model_stub::FIRST_CHUNK)),
        "the forked turn is on screen after resume: {restored:#?}"
    );
    assert_eq!(
        fixture.session_files().len(),
        2,
        "resuming the fork reopens its file instead of starting another"
    );
    assert_eq!(
        fixture.stub.started(),
        1,
        "fork and resume make no model request"
    );
    fixture.stop(&fork_id).await;
}

/// Drives a real `pi` through the installed package: launch, detect, input,
/// stop, resume, fork, resume of the fork and the existence check against
/// Pi's own session files.
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
    let (id, reference, file) = launch_and_converse(&fixture).await;
    check_resume(&fixture, &id).await;
    check_fork(&fixture, &id, &reference, &file).await;

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

/// Pi's idle editor frame is its input readiness, so a launch with initial
/// input waits for the real editor and the prompt reaches the model once.
///
/// Needs a `pi` on `PATH` and `POHUNEK_PI_E2E=1`; the model is a loopback
/// stub.
#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn a_real_pi_launch_submits_its_initial_input_once_the_editor_shows() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    fixture.write_profile();
    fixture.stub.open_gate();

    let cwd = fixture.harness.env.cwd().to_path_buf();
    let (code, launched) = fixture
        .harness
        .json(&[
            "session",
            "new",
            "--agent",
            PROFILE,
            "--cwd",
            path_str(&cwd),
            "--name",
            "initial-input",
            "--input",
            PROMPT,
        ])
        .await;
    assert_eq!(code, 0, "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    fixture
        .wait_screen_line(
            &id,
            &format!("{}{}", model_stub::FIRST_CHUNK, model_stub::SECOND_CHUNK),
        )
        .await;
    assert_eq!(fixture.stub.started(), 1, "one model request");
    assert_eq!(
        fixture.stub.bodies_containing(PROMPT).len(),
        1,
        "the model request carries the initial input"
    );

    fixture.stop(&id).await;
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

/// Name of the extension command that opens a confirm dialog.
const CONFIRM_COMMAND: &str = "pohunek-confirm";

/// Writes an agent-directory extension whose command opens a confirm dialog
/// and writes `marker` only when the dialog is answered Yes.
fn write_confirm_extension(fixture: &Fixture, marker: &Path) {
    let extensions = fixture.harness.env.home().join(".pi/agent/extensions");
    fs::create_dir_all(&extensions).expect("create the extensions directory");
    fs::write(
        extensions.join("confirm.ts"),
        format!(
            "import {{ writeFileSync }} from \"node:fs\";\n\
             export default function (pi) {{\n\
               pi.registerCommand({CONFIRM_COMMAND:?}, {{\n\
                 description: \"Ask before writing the marker\",\n\
                 handler: async (_args, ctx) => {{\n\
                   if (await ctx.ui.confirm(\"Allow tool call?\", \"bash: touch marker\")) {{\n\
                     writeFileSync({marker:?}, \"approved\\n\");\n\
                   }}\n\
                 }},\n\
               }});\n\
             }}\n",
            marker = marker.to_str().expect("UTF-8 marker path"),
        ),
    )
    .expect("write the confirm extension");
}

/// Asks once, for `activity`, and returns whether it matched within the wait.
async fn activity_matches(fixture: &Fixture, id: &str, activity: &str) -> bool {
    let (code, waited) = fixture
        .harness
        .json(&[
            "session",
            "wait",
            id,
            "--activity",
            activity,
            "--timeout-ms",
            "3000",
        ])
        .await;
    assert_eq!(code, 0, "{waited}");
    waited["ok"]["reason"] == "activity_matched"
}

/// A dialog of a real Pi extension waits for the owner: the session reads
/// `blocked` from the screen (and never `idle`) until the editor returns, and
/// a project with its own extensions reads `blocked` at startup while Pi asks
/// whether to trust it. Neither prompt is answered, so neither marker exists.
///
/// Needs a `pi` on `PATH` and `POHUNEK_PI_E2E=1`; no model is involved.
#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn a_real_pi_dialog_or_trust_prompt_reads_blocked_never_idle() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    fixture.write_profile();
    let marker = fixture.harness.env.root().join("approved");
    write_confirm_extension(&fixture, &marker);

    let launched = fixture.new_session("dialog").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;
    let (code, sent) = fixture
        .harness
        .json(&["session", "input", &id, &format!("/{CONFIRM_COMMAND}")])
        .await;
    assert_eq!(code, 0, "{sent}");
    let blocked = fixture.wait_activity(&id, "blocked").await;
    assert_eq!(blocked["state_source"], "screen", "{blocked}");
    let lines = fixture.wait_screen_line(&id, "Allow tool call?").await;
    assert!(
        !activity_matches(&fixture, &id, "idle").await,
        "the open dialog is never idle: {lines:#?}"
    );
    fixture.stop(&id).await;

    // A project with its own extension makes Pi ask for trust before it
    // starts; the extension would write the marker when it loads.
    let project = fixture.harness.env.root().join("project");
    fs::create_dir_all(project.join(".pi/extensions")).expect("create the project extensions");
    fs::write(
        project.join(".pi/extensions/project.ts"),
        format!(
            "import {{ writeFileSync }} from \"node:fs\";\n\
             writeFileSync({marker:?}, \"loaded\\n\");\n\
             export default function () {{}}\n",
            marker = marker.to_str().expect("UTF-8 marker path"),
        ),
    )
    .expect("write the project extension");
    let (code, launched) = fixture.launch(PROFILE, "trust", &project).await;
    assert_eq!(code, 0, "{launched}");
    let trust = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let blocked = fixture.wait_activity(&trust, "blocked").await;
    assert_eq!(blocked["state_source"], "screen", "{blocked}");
    fixture
        .wait_screen_line(&trust, "Trust project folder?")
        .await;
    assert!(!activity_matches(&fixture, &trust, "idle").await);
    fixture.stop(&trust).await;

    assert!(!marker.exists(), "no prompt was answered");
    fixture.harness.stop().await;
}

/// A Pi told to keep its sessions in `PI_CODING_AGENT_SESSION_DIR` writes
/// them directly in that directory, and the existence check follows it, so the
/// session resumes from there.
#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn a_real_pi_session_in_the_session_dir_variable_resumes() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    let store = fixture.harness.env.root().join("pi-sessions");
    fs::create_dir_all(&store).expect("create the session directory");
    fixture.write_named_profile(
        PROFILE,
        "",
        &format!(
            "PI_CODING_AGENT_SESSION_DIR = \"{}\"\n",
            store.to_str().expect("UTF-8 path")
        ),
    );

    let launched = fixture.new_session("session-dir").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    let reference = launched["native_session_id"]
        .as_str()
        .expect("assigned reference")
        .to_owned();
    fixture.wait_activity(&id, "idle").await;
    let (code, sent) = fixture
        .harness
        .json(&["session", "input", &id, PROMPT])
        .await;
    assert_eq!(code, 0, "{sent}");
    fixture.wait_activity(&id, "working").await;
    fixture.stub.open_gate();
    fixture.wait_activity(&id, "idle").await;
    fixture
        .wait_screen_line(
            &id,
            &format!("{}{}", model_stub::FIRST_CHUNK, model_stub::SECOND_CHUNK),
        )
        .await;
    let suffix = format!("_{reference}.jsonl");
    let written: Vec<PathBuf> = fs::read_dir(&store)
        .expect("read the session directory")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert!(
        written.iter().any(|path| path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(&suffix))),
        "Pi wrote the session directly in the variable's directory: {written:?}"
    );
    assert!(
        fixture.session_files().is_empty(),
        "nothing in the agent directory's store"
    );

    check_resume(&fixture, &id).await;
    fixture.stop(&id).await;
    fixture.harness.stop().await;
}

/// A launch whose Pi would keep its sessions where the existence check cannot
/// follow (a `sessionDir` setting, the `--session-dir` flag) is refused before
/// Pi starts, with the variable that resolves it; the variable wins over the
/// setting, so setting it launches.
#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn a_real_pi_with_its_sessions_elsewhere_is_refused_at_launch() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    let store = fixture.harness.env.root().join("pi-sessions");
    fs::create_dir_all(&store).expect("create the session directory");
    let store_path = store.to_str().expect("UTF-8 path");
    let settings = fixture.harness.env.home().join(".pi/agent/settings.json");
    let mut configured: Value =
        serde_json::from_str(&fs::read_to_string(&settings).expect("read settings"))
            .expect("settings JSON");
    configured["sessionDir"] = Value::String(store_path.to_owned());
    fs::write(&settings, configured.to_string()).expect("write settings");
    fixture.write_profile();
    fixture.write_named_profile(
        "pi-flag",
        &format!("args = [\"--session-dir\", \"{store_path}\"]\n"),
        "",
    );
    fixture.write_named_profile(
        "pi-variable",
        "",
        &format!("PI_CODING_AGENT_SESSION_DIR = \"{store_path}\"\n"),
    );
    let cwd = fixture.harness.env.cwd().to_path_buf();

    for (profile, case) in [
        (PROFILE, "sessionDir setting"),
        ("pi-flag", "--session-dir"),
    ] {
        let (code, refused) = fixture.launch(profile, "refused", &cwd).await;
        assert_eq!(code, 1, "{case}: {refused}");
        assert_eq!(
            refused["err"]["code"], "agent_native_store_relocated",
            "{case}: {refused}"
        );
        assert!(
            refused["err"]["recover"]
                .as_str()
                .is_some_and(|hint| hint.contains("PI_CODING_AGENT_SESSION_DIR")),
            "{case}: {refused}"
        );
    }
    let (code, listed) = fixture.harness.json(&["session", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(
        listed["ok"].as_array().map(Vec::len),
        Some(0),
        "a refused launch leaves no session: {listed}"
    );

    let (code, launched) = fixture.launch("pi-variable", "variable", &cwd).await;
    assert_eq!(code, 0, "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    fixture.wait_activity(&id, "idle").await;
    fixture.stop(&id).await;
    fixture.harness.stop().await;
}

/// The ordered root program-status records of the session's retained output.
async fn program_status_of(fixture: &Fixture, id: &str) -> Vec<String> {
    let (code, output) = fixture.harness.json(&["session", "output", id]).await;
    assert_eq!(code, 0, "{output}");
    capture::program_status_records(&capture::output_bytes(&output))
}

#[tokio::test]
#[ignore = "needs a real `pi` on PATH; run with POHUNEK_PI_E2E=1 and --ignored"]
async fn explore_pi_program_status() {
    require_real_pi();
    let fixture = Fixture::start().await;
    fixture.install().await;
    fixture.write_profile();
    let marker = fixture.harness.env.root().join("approved");
    write_confirm_extension(&fixture, &marker);

    let launched = fixture.new_session("explore").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    let idle = fixture.wait_activity(&id, "idle").await;
    eprintln!("IDLE {} {:?}", idle["state_source"], program_status_of(&fixture, &id).await);
    let (code, sent) = fixture.harness.json(&["session", "input", &id, PROMPT]).await;
    assert_eq!(code, 0, "{sent}");
    let working = fixture.wait_activity(&id, "working").await;
    eprintln!("WORKING {} {:?}", working["state_source"], program_status_of(&fixture, &id).await);
    fixture.stub.open_gate();
    let idle = fixture.wait_activity(&id, "idle").await;
    eprintln!("IDLE2 {} {:?}", idle["state_source"], program_status_of(&fixture, &id).await);
    let (code, sent) = fixture
        .harness
        .json(&["session", "input", &id, &format!("/{CONFIRM_COMMAND}")])
        .await;
    assert_eq!(code, 0, "{sent}");
    let blocked = fixture.wait_activity(&id, "blocked").await;
    eprintln!("BLOCKED {} {:?}", blocked["state_source"], program_status_of(&fixture, &id).await);
    fixture.stop(&id).await;
    fixture.harness.stop().await;
}
