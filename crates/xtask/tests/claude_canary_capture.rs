//! Scenarios for `claude canary capture`, driven through the real `xtask`
//! binary at the component boundary against the stand-in `claude` fixture
//! (`tests/fixtures/claude_canary/stand-in-claude.sh`). The stand-in proves the
//! driver, the record reader and the capture schema, not Claude.
//!
//! - C1 every scenario: exit 0, and `compare` against the checked-in expected
//!   table exits 3 with every row but the login row VERIFIED; each profile
//!   launches with its arguments.
//! - C2 the permission record carries a `msg` tail and arrives split across
//!   two writes after a split multi-byte character: the capture holds
//!   `state=blocked:kind=permission`, and neither the capture nor the daemon
//!   log holds the tail, its base64 or its decoded text.
//! - C3 a relative `--claude`: exit 2.
//! - C4 an account scenario without `--claude-config-dir`: exit 2 naming it.
//! - C5 an agent that never answers: exit 4, the partial capture holds the
//!   rows captured before, stderr names the step (the prompt's input wait,
//!   and the record wait `--step-timeout-secs` bounds).
//! - C7 a workdir the account has not trusted, its dialog drawn after a blank
//!   first screen: exit 4 telling the operator to trust it first, and nothing
//!   is typed into the dialog.
//!
//! C6 (`plugin prove` unchanged by the shared host) is `plugin_release.rs`.
//! Every run that starts a daemon must leave no process behind.

// Rust guideline compliant 2026-11-14

use std::path::{Path, PathBuf};
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use serde_json::Value;

/// The `msg` tail of the stand-in's permission record, base64 and decoded.
const MESSAGE_BASE64: &str = "U0VDUkVULVBST01QVC1URVhU";
const MESSAGE_TEXT: &str = "SECRET-PROMPT-TEXT";

/// Step bound of the scenarios whose agent answers: the stand-in answers
/// within two seconds, the bound only ends a broken run.
const ANSWERING_STEP_TIMEOUT: &str = "60";

/// Step bound of the scenarios that wait for an answer that never comes.
const SILENT_STEP_TIMEOUT: &str = "3";

fn s(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn checkout() -> PathBuf {
    pohunek_test_support::manifest_dir()
        .join("../..")
        .canonicalize()
        .expect("the checkout")
}

/// Random characters of the scratch directory's name: short, because the
/// session worker sockets below it must fit the platform limit.
const SCRATCH_RANDOM_BYTES: usize = 3;

/// The directory for the capture's isolation roots, when the environment
/// root cannot hold them. On Linux they go into the environment root, so the
/// survivor scan sees every process of the run by path. macOS allows 4 bytes
/// less per socket path and its environment root (`/private/tmp/ph-XXXXXX`)
/// leaves no room for the session worker sockets, so a private directory
/// with a short name in the shared temporary root holds them there.
fn scratch_dir() -> Option<tempfile::TempDir> {
    cfg!(target_os = "macos").then(|| {
        pohunek_test_support::tempdir_with_random_bytes(SCRATCH_RANDOM_BYTES)
            .expect("create the scratch directory")
    })
}

/// A hermetic environment with the checkout marker `xtask` requires. The
/// capture's isolation root goes below the environment root (`--scratch-parent`),
/// which `TestEnv` keeps short enough for the nested worker sockets.
struct Capture {
    env: TestEnv,
    /// The macOS directory for the capture's isolation roots, removed with
    /// the test; see [`scratch_dir`].
    scratch: Option<tempfile::TempDir>,
}

impl Capture {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic test environment");
        let marker = env.cwd().join("crates/xtask/Cargo.toml");
        std::fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
        std::fs::write(marker, "").expect("marker");
        let scratch = scratch_dir();
        Self { env, scratch }
    }

    /// A git work directory holding the stand-in's test-only mode switches.
    fn workdir(&self, switches: &[&str]) -> PathBuf {
        let workdir = self.env.cwd().join("work");
        std::fs::create_dir(&workdir).expect("mkdir");
        let status = self
            .env
            .git_command()
            .args(["init", "-q"])
            .current_dir(&workdir)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init");
        for switch in switches {
            std::fs::write(workdir.join(switch), "").expect("write the switch");
        }
        workdir
    }

    /// A non-empty config home: the stand-in takes it as a logged-in account.
    fn config_dir(&self) -> PathBuf {
        let dir = self.env.cwd().join("claude-config");
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::write(dir.join("settings.json"), "{}").expect("seed the config");
        dir
    }

    /// #2368: the daemon log in `--out` confirms `--local-only` and shows no
    /// overlay listener attempt.
    fn assert_local_only_daemon(&self) {
        let log =
            std::fs::read_to_string(self.out().join("daemon.log")).expect("read the daemon log");
        assert!(
            log.contains("remote control listener disabled by --local-only"),
            "{log}"
        );
        for overlay in [
            "remote control listener not bound",
            "over overlay",
            "overlay CLI",
        ] {
            assert!(!log.contains(overlay), "the daemon log holds `{overlay}`");
        }
    }

    /// The launch lines the stand-in appended beside the config home.
    fn argv_log(&self) -> String {
        std::fs::read_to_string(self.env.cwd().join("argv.log")).unwrap_or_default()
    }

    /// The stand-in, copied below the environment root so the survivor scan
    /// recognizes its processes by path.
    fn claude(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        // Not named `claude`: the package's process matcher would take the
        // stand-in's short-lived forks for the agent and reset the activity
        // its records set.
        let path = self.env.root().join("canary-agent");
        std::fs::copy(
            pohunek_test_support::manifest_dir()
                .join("tests/fixtures/claude_canary/stand-in-claude.sh"),
            &path,
        )
        .expect("copy the stand-in");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// A `release test-runtime --runtime claude` tree with the workspace
    /// binaries copied in, the shape the runbook describes.
    fn release_dir(&self) -> PathBuf {
        let dir = self.env.cwd().join("release");
        std::fs::create_dir(&dir).expect("mkdir");
        let output = self
            .env
            .command(pohunek_test_support::bin_exe("xtask"))
            .current_dir(checkout())
            .args([
                "release",
                "test-runtime",
                "--output",
                s(&dir),
                "--runtime",
                "claude",
            ])
            .output()
            .expect("run xtask");
        assert!(
            output.status.success(),
            "release test-runtime: {}",
            stderr_of(&output)
        );
        let bin_dir = pohunek_test_support::bin_exe("xtask")
            .parent()
            .expect("the xtask binary has a directory")
            .to_path_buf();
        for name in ["pohunek", "pohunekd", "pohunek-sessiond"] {
            let source = bin_dir.join(name);
            assert!(
                source.is_file(),
                "{} is missing; build it with `cargo build -p pohunek-cli -p pohunek-daemon -p pohunek-session-worker`",
                source.display()
            );
            std::fs::copy(&source, dir.join(name)).expect("copy the binary");
        }
        dir
    }

    /// The directory the capture's isolation root goes into.
    fn scratch_base(&self) -> PathBuf {
        self.scratch.as_ref().map_or_else(
            || self.env.root().to_path_buf(),
            |dir| dir.path().to_path_buf(),
        )
    }

    fn out(&self) -> PathBuf {
        self.env.cwd().join("out")
    }

    fn xtask(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .current_dir(self.env.cwd())
            .args(args)
            .output()
            .expect("run xtask")
    }

    /// Runs `capture` with the given inputs and extra flags.
    fn capture(
        &self,
        release: &Path,
        claude: &Path,
        config: Option<&Path>,
        workdir: &Path,
        extra: &[&str],
    ) -> Output {
        let out = self.out();
        let scratch = self.scratch_base();
        let mut args = vec![
            "claude",
            "canary",
            "capture",
            "--release-dir",
            s(release),
            "--claude",
            s(claude),
            "--workdir",
            s(workdir),
            "--out",
            s(&out),
            "--scratch-parent",
            s(&scratch),
        ];
        if let Some(config) = config {
            args.extend(["--claude-config-dir", s(config)]);
        }
        args.extend(extra);
        self.xtask(&args)
    }

    fn capture_document(&self) -> Value {
        let text = std::fs::read_to_string(self.out().join("capture.json"))
            .expect("capture.json is written");
        serde_json::from_str(&text).expect("capture.json is JSON")
    }

    /// No process of the run survives it: the daemon and session workers run
    /// from the isolation root and the agent is the stand-in copy, all below
    /// the environment root. A process the run stopped may take a moment to
    /// disappear, so the scan polls until none is left.
    #[cfg(target_os = "linux")]
    fn assert_no_survivors(&self) {
        use std::os::unix::ffi::OsStrExt as _;
        let root = self.env.root().as_os_str().as_bytes().to_vec();
        let mut reported = false;
        pohunek_test_support::wait::poll_until("every process of the capture to exit", || {
            let survivors = processes_holding(&root);
            if survivors.is_empty() {
                return Some(());
            }
            if !reported {
                eprintln!("processes of the capture are still running: {survivors:?}");
                reported = true;
            }
            None
        });
    }

    /// The scan reads `/proc`; other platforms rely on the Linux runs.
    #[cfg(not(target_os = "linux"))]
    #[expect(
        clippy::unused_self,
        reason = "the same call site as the Linux scan, which reads the root"
    )]
    fn assert_no_survivors(&self) {}
}

#[test]
fn unknown_scenario_is_refused() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let output = capture.capture(
        Path::new("/nonexistent-release-dir"),
        &capture.claude(),
        None,
        &workdir,
        &["--scenario", "not_a_scenario"],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("unknown canary scenario `not_a_scenario`"),
        "{}",
        stderr_of(&output)
    );
}

/// C3.
#[test]
fn relative_claude_is_refused() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let output = capture.capture(
        Path::new("/nonexistent-release-dir"),
        Path::new("relative/claude"),
        None,
        &workdir,
        &["--scenario", "login_screen"],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("`--claude` must be an absolute path"),
        "{}",
        stderr_of(&output)
    );
    assert!(
        !capture.out().exists(),
        "nothing is written before the inputs pass"
    );
}

#[test]
fn relative_out_is_refused() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let output = capture.xtask(&[
        "claude",
        "canary",
        "capture",
        "--release-dir",
        "/nonexistent-release-dir",
        "--claude",
        s(&capture.claude()),
        "--workdir",
        s(&workdir),
        "--out",
        "relative-out",
        "--scenario",
        "login_screen",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("`--out` must be an absolute path"),
        "{}",
        stderr_of(&output)
    );
    assert!(!capture.env.cwd().join("relative-out").exists());
}

/// C4.
#[test]
fn account_scenario_without_config_dir_is_refused() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let output = capture.capture(
        Path::new("/nonexistent-release-dir"),
        &capture.claude(),
        None,
        &workdir,
        &["--scenario", "idle_after_turn"],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("--claude-config-dir"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn non_git_workdir_is_refused() {
    let capture = Capture::new();
    let workdir = capture.env.cwd().join("plain");
    std::fs::create_dir(&workdir).expect("mkdir");
    let output = capture.capture(
        Path::new("/nonexistent-release-dir"),
        &capture.claude(),
        None,
        &workdir,
        &["--scenario", "login_screen"],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("is not a git directory"),
        "{}",
        stderr_of(&output)
    );
}

/// The scratch parent check matches the daemon's own socket check: the
/// longest parent the capture accepts starts a session (the daemon validates
/// the worker socket at spawn), and one byte more is refused before anything
/// starts.
#[test]
fn scratch_parent_at_the_socket_limit_runs_and_one_byte_more_is_refused() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let claude = capture.claude();
    let base = capture.scratch_base();
    let parent_of = |length: usize| -> PathBuf {
        let name_length = length
            .checked_sub(base.as_os_str().len() + 1)
            .filter(|name_length| *name_length > 0)
            .unwrap_or_else(|| {
                panic!(
                    "`{}` is too long for a {length}-byte parent",
                    base.display()
                )
            });
        let parent = base.join("x".repeat(name_length));
        std::fs::create_dir_all(&parent).expect("mkdir");
        parent
    };
    let refused = |parent: &Path| -> bool {
        let output = capture.xtask(&[
            "claude",
            "canary",
            "capture",
            "--release-dir",
            "/nonexistent-release-dir",
            "--claude",
            s(&claude),
            "--workdir",
            s(&workdir),
            "--out",
            s(&capture.env.cwd().join("out-probe")),
            "--scratch-parent",
            s(parent),
            "--scenario",
            "login_screen",
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "stderr: {}",
            stderr_of(&output)
        );
        stderr_of(&output).contains("too long for the session sockets")
    };
    let shortest = base.as_os_str().len() + 2;
    assert!(
        !refused(&parent_of(shortest)),
        "the base leaves room for a parent"
    );
    // One directory name below the base holds the extra length; a name has
    // at most 255 bytes.
    let longest = base.as_os_str().len() + 1 + 255;
    let limit = (shortest..=longest)
        .take_while(|length| !refused(&parent_of(*length)))
        .last()
        .expect("a parent length is accepted");
    assert!(refused(&parent_of(limit + 1)));

    let release = capture.release_dir();
    let output = capture.xtask(&[
        "claude",
        "canary",
        "capture",
        "--release-dir",
        s(&release),
        "--claude",
        s(&claude),
        "--workdir",
        s(&workdir),
        "--out",
        s(&capture.out()),
        "--scratch-parent",
        s(&parent_of(limit)),
        "--scenario",
        "login_screen",
        "--step-timeout-secs",
        ANSWERING_STEP_TIMEOUT,
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();
    assert!(capture.capture_document()["scenarios"]["login_screen"].is_object());
}

/// C1 and C2.
#[test]
fn every_scenario_matches_the_expected_table() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let config = capture.config_dir();
    let release = capture.release_dir();
    let output = capture.capture(
        &release,
        &capture.claude(),
        Some(&config),
        &workdir,
        &["--step-timeout-secs", ANSWERING_STEP_TIMEOUT],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();

    let expected = checkout().join("compat/claude/canary/expected.json");
    let compared = capture.xtask(&[
        "claude",
        "canary",
        "compare",
        "--expected",
        s(&expected),
        "--capture",
        s(&capture.out().join("capture.json")),
    ]);
    let report = String::from_utf8_lossy(&compared.stdout).into_owned();
    assert_eq!(
        compared.status.code(),
        Some(0),
        "compare: {report}{}",
        stderr_of(&compared)
    );
    for row in [
        "idle_after_turn",
        "permission_prompt",
        "mcp_elicitation",
        "login_screen",
        "running_turn",
    ] {
        assert!(
            report
                .lines()
                .any(|line| line.starts_with(row) && line.ends_with("| VERIFIED")),
            "{row} is VERIFIED: {report}"
        );
    }

    let document = capture.capture_document();
    assert_eq!(document["claude_version"], "2.1.322 (Claude Code)");
    assert_eq!(
        document["scenarios"]["permission_prompt"]["final_record"],
        "state=blocked:kind=permission"
    );
    assert_eq!(
        document["scenarios"]["running_turn"]["final_record"],
        "state=working"
    );
    capture.assert_local_only_daemon();
    assert_eq!(
        theme_input(&workdir),
        "\r",
        "the login scenario answers the theme picker with one Enter"
    );
    for file in ["capture.json", "daemon.log"] {
        let text = std::fs::read_to_string(capture.out().join(file)).expect("read the output file");
        for leaked in ["msg=", MESSAGE_BASE64, MESSAGE_TEXT] {
            assert!(!text.contains(leaked), "{file} holds `{leaked}`");
        }
    }

    let argv = capture.argv_log();
    assert!(
        argv.lines()
            .any(|line| line.contains("--permission-mode=default")),
        "{argv}"
    );
    let mcp = argv
        .lines()
        .find(|line| line.contains("--mcp-config="))
        .unwrap_or_else(|| panic!("the MCP profile launched: {argv}"));
    assert!(
        mcp.contains("--allowedTools=mcp__canary__ask") && mcp.contains("--strict-mcp-config"),
        "{mcp}"
    );
}

/// One `--scenario` yields exactly its row; the login row keeps the record
/// the login screen emitted, also when a blank screen and the first-run theme
/// picker come first.
#[test]
fn one_scenario_holds_exactly_its_row() {
    let capture = Capture::new();
    let workdir = capture.workdir(&["login-record", "slow-start"]);
    let release = capture.release_dir();
    let output = capture.capture(
        &release,
        &capture.claude(),
        None,
        &workdir,
        &[
            "--scenario",
            "login_screen",
            "--step-timeout-secs",
            ANSWERING_STEP_TIMEOUT,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();
    let document = capture.capture_document();
    let scenarios = document["scenarios"].as_object().expect("scenarios");
    assert_eq!(scenarios.keys().collect::<Vec<_>>(), ["login_screen"]);
    assert_eq!(
        scenarios["login_screen"]["final_record"],
        "state=blocked:kind=auth"
    );
    assert_eq!(
        scenarios["login_screen"]["records"],
        serde_json::json!(["state=blocked:kind=auth"])
    );
    assert_eq!(theme_input(&workdir), "\r");
}

/// An account whose config home has not finished Claude's first run shows the
/// theme picker; the capture refuses it and types nothing into it.
#[test]
fn an_account_scenario_refuses_the_first_run_theme_picker() {
    let capture = Capture::new();
    let workdir = capture.workdir(&[]);
    let config = capture.env.cwd().join("fresh-config");
    std::fs::create_dir(&config).expect("mkdir");
    let release = capture.release_dir();
    let output = capture.capture(
        &release,
        &capture.claude(),
        Some(&config),
        &workdir,
        &[
            "--scenario",
            "idle_after_turn",
            "--step-timeout-secs",
            ANSWERING_STEP_TIMEOUT,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("first-run theme picker"),
        "{}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();
    assert_eq!(theme_input(&workdir), "", "no key reached the picker");
}

/// C5.
#[test]
fn silent_agent_stops_with_the_partial_capture() {
    let capture = Capture::new();
    let workdir = capture.workdir(&["silent"]);
    let config = capture.config_dir();
    let release = capture.release_dir();
    let output = capture.capture(
        &release,
        &capture.claude(),
        Some(&config),
        &workdir,
        &[
            "--scenario",
            "login_screen",
            "--scenario",
            "running_turn",
            "--step-timeout-secs",
            SILENT_STEP_TIMEOUT,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("step `send the `running_turn` prompt"),
        "stderr names the step: {}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();
    let document = capture.capture_document();
    let scenarios = document["scenarios"].as_object().expect("scenarios");
    assert_eq!(scenarios.keys().collect::<Vec<_>>(), ["login_screen"]);
    assert!(capture.out().join("daemon.log").is_file());
}

/// C5 at the record wait: a prompt sent without an input wait and never
/// answered runs into `--step-timeout-secs`.
#[test]
fn silent_agent_stops_at_the_record_wait() {
    let capture = Capture::new();
    let workdir = capture.workdir(&["silent"]);
    let config = capture.config_dir();
    let release = capture.release_dir();
    let output = capture.capture(
        &release,
        &capture.claude(),
        Some(&config),
        &workdir,
        &[
            "--scenario",
            "permission_prompt",
            "--step-timeout-secs",
            SILENT_STEP_TIMEOUT,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "stderr: {}",
        stderr_of(&output)
    );
    let step = format!("step `wait for a `blocked` record` timed out after {SILENT_STEP_TIMEOUT}s");
    assert!(
        stderr_of(&output).contains(&step),
        "stderr names the step: {}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();
    assert_eq!(
        capture.capture_document()["scenarios"],
        serde_json::json!({})
    );
}

/// C7.
#[test]
fn untrusted_workdir_stops_before_any_prompt() {
    let capture = Capture::new();
    // The dialog appears after a blank first screen, as Claude draws it.
    let workdir = capture.workdir(&["untrusted", "slow-start"]);
    let config = capture.config_dir();
    let release = capture.release_dir();
    let output = capture.capture(
        &release,
        &capture.claude(),
        Some(&config),
        &workdir,
        &[
            "--scenario",
            "idle_after_turn",
            "--step-timeout-secs",
            ANSWERING_STEP_TIMEOUT,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "stderr: {}",
        stderr_of(&output)
    );
    let trust = format!(
        "trust {} in Claude for this config first",
        workdir.display()
    );
    assert!(
        stderr_of(&output).contains(&trust),
        "{}",
        stderr_of(&output)
    );
    capture.assert_no_survivors();
    let document = capture.capture_document();
    assert_eq!(document["scenarios"], serde_json::json!({}));
    assert_eq!(
        capture.argv_log().lines().count(),
        1,
        "the agent launched once"
    );
    let typed = std::fs::read(capture.env.cwd().join("input.log")).unwrap_or_default();
    assert!(
        typed.is_empty(),
        "nothing is typed into the trust dialog: {typed:?}"
    );
}

/// The bytes the stand-in read on its theme picker in `workdir`.
fn theme_input(workdir: &Path) -> String {
    std::fs::read_to_string(workdir.join("theme-input.log")).unwrap_or_default()
}

/// The processes whose command line or environment holds `root`.
#[cfg(target_os = "linux")]
fn processes_holding(root: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("read /proc").flatten() {
        let name = entry.file_name();
        if !name
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let holds_root = ["cmdline", "environ"].iter().any(|file| {
            std::fs::read(entry.path().join(file))
                .is_ok_and(|bytes| bytes.windows(root.len()).any(|window| window == root))
        });
        if holds_root {
            let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            found.push(format!(
                "{} {}",
                name.to_string_lossy(),
                String::from_utf8_lossy(&cmdline)
            ));
        }
    }
    found
}
