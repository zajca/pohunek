//! `pohunek plugin run` and `plugin profile create --program`.
//!
//! The refusals run the `pohunek` binary with a null stdin and no daemon
//! interaction. The success and status scenarios run the binary on a pseudo
//! terminal against a real daemon whose fixture package declares one setup
//! entry: a shell script that reports the names of the variables it inherited
//! (never their values) and the arguments it received, then exits as the owner
//! asked. The test environment sets decoy variables that must not arrive.

// Rust guideline compliant 2026-11-23

#![cfg(unix)]

use std::fs;
use std::io::Read as _;
use std::process::{Output, Stdio};
use std::thread;
use std::time::Duration;

use package::{build_archive, read_archive, ArchiveEntry, Limits};
use pohunek_test_support::bin_exe;
use pohunek_test_support::wait::guard;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Built, Harness};

/// Package id of the fixture that declares the setup entry.
const PACKAGE: &str = "acme.runtime.setup";

/// Runtime id the fixture package serves; `plugin run` accepts it as an alias.
const RUNTIME: &str = "setupfix";

/// Name of the setup entry the fixture declares.
const SETUP: &str = "accounts";

/// Fixed argument the descriptor places before the owner's arguments.
const FIXED_ARG: &str = "--fixed";

/// Owner argument that makes the fixture exit with [`EXIT_STATUS_CODE`].
const EXIT_ARG: &str = "exit-seven";

/// Exit status the fixture chooses for [`EXIT_ARG`].
const EXIT_STATUS_CODE: u32 = 7;

/// Owner argument that makes the fixture terminate itself with SIGTERM.
const SIGNAL_ARG: &str = "term";

/// Exit status of a program ended by SIGTERM, `128 + 15` in the shell convention.
const SIGTERM_STATUS: u32 = 143;

/// Exit status of a usage error.
const USAGE_STATUS: i32 = 2;

/// Exit status of a refusal that is not a usage error.
const FAILURE_STATUS: i32 = 1;

/// Variables the test sets on the CLI that must never reach the setup program.
const DECOY_ENV: [(&str, &str); 2] = [
    ("POHUNEK_DECOY_FOR_TEST", "1"),
    ("ACME_SERVICE_TOKEN", "decoy-value"),
];

/// Variables the setup program may see that pohunek itself sets for it.
const SETUP_ENV: [&str; 3] = [
    "POHUNEK_SETUP_PACKAGE",
    "POHUNEK_SETUP_PACKAGE_ROOT",
    "POHUNEK_SETUP_DIGEST",
];

/// Columns and rows of the pseudo terminal the CLI runs on.
const TERMINAL_COLS: u16 = 120;
const TERMINAL_ROWS: u16 = 40;

/// Pause between polls of a child that has not exited yet.
const POLL_STEP: Duration = Duration::from_millis(50);

/// Polls before a child that has not exited is reported as hung.
const POLL_LIMIT: u32 = 1200;

/// The fixture setup program. It prints the argument vector, then the names of
/// the variables it inherited, then acts on the first owner argument.
const SETUP_SCRIPT: &str = r#"#!/bin/sh
printf 'ARGS'
for arg in "$@"; do printf ' [%s]' "$arg"; done
printf '\n'
awk 'BEGIN { for (name in ENVIRON) print "ENV " name }' | sort
case " $* " in
*" exit-seven "*) exit 7 ;;
*" term "*) kill -TERM $$ ;;
esac
exit 0
"#;

fn descriptor() -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE}"
version = "1.0.0"
runtime_api = 1
requires = ["package-setup-v1"]

[runtime]
id = "{RUNTIME}"
name = "Setup fixture"
program = "/bin/sh"
args = []
detect_manifest = "detect.toml"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]
assign_fork = true

[native_reference.existence]
check = "none"

[setup.{SETUP}]
program = "bin/accounts.sh"
args = ["{FIXED_ARG}"]
description = "Fixture setup entry"
"#
    )
}

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

impl Harness {
    /// Writes the fixture archive and returns it with its digest.
    fn setup_archive(&self) -> Built {
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: descriptor().into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
            ArchiveEntry {
                path: "bin/accounts.sh".to_owned(),
                contents: SETUP_SCRIPT.as_bytes().to_vec(),
                executable: true,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let path = self.env.root().join("setup-fixture.tar.zst");
        fs::write(&path, bytes).expect("write the archive");
        Built { path, digest }
    }

    /// Installs the fixture enabled and selected through the CLI.
    async fn install_setup_package(&self) {
        let built = self.setup_archive();
        let (code, document) = self
            .json(&[
                "plugin",
                "install",
                path_str(&built.path),
                "--sha256",
                built.digest.as_str(),
                "--yes",
            ])
            .await;
        assert_eq!(code, 0, "{document}");
        assert_eq!(document["ok"]["status"], "installed", "{document}");
    }
}

/// Runs `pohunek` with the given arguments, a null stdin and the hermetic
/// environment plus `extra`.
async fn run_detached(harness: &Harness, arguments: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = harness.env.tokio_command(bin_exe("pohunek"));
    command
        .args(arguments)
        .envs(extra.iter().copied())
        .stdin(Stdio::null());
    guard("the pohunek binary to finish", command.output())
        .await
        .expect("run the pohunek binary")
}

/// Runs `pohunek` on a pseudo terminal, so stdin and stderr are terminals, and
/// returns the exit status and the transcript with CRLF line endings folded.
fn run_on_terminal(harness: &Harness, arguments: &[&str]) -> (u32, String) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: TERMINAL_ROWS,
            cols: TERMINAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let std_command = harness.env.command(bin_exe("pohunek"));
    let mut command = CommandBuilder::new(std_command.get_program());
    command.env_clear();
    for (name, value) in std_command.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    for (name, value) in DECOY_ENV {
        command.env(name, value);
    }
    command.args(arguments);
    command.cwd(harness.env.cwd());
    let mut child = pair.slave.spawn_command(command).expect("start pohunek");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let transcript = thread::spawn(move || {
        let mut output = Vec::new();
        // The read fails with EIO once the child has closed the terminal.
        let _ = reader.read_to_end(&mut output);
        output
    });
    let mut status = None;
    for _ in 0..POLL_LIMIT {
        if let Some(exited) = child.try_wait().expect("poll pohunek") {
            status = Some(exited);
            break;
        }
        thread::sleep(POLL_STEP);
    }
    let status = status.unwrap_or_else(|| {
        let _ = child.kill();
        panic!("pohunek {arguments:?} did not exit");
    });
    let output = transcript.join().expect("transcript thread");
    let text = String::from_utf8_lossy(&output).replace("\r\n", "\n");
    (status.exit_code(), text)
}

/// The environment names the setup program reported, one per `ENV ` line.
fn reported_names(transcript: &str) -> Vec<&str> {
    transcript
        .lines()
        .filter_map(|line| line.strip_prefix("ENV "))
        .collect()
}

#[tokio::test]
async fn refused_inside_a_session_before_anything_runs() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let output = run_detached(
        &harness,
        &["plugin", "run", PACKAGE, SETUP],
        &[("POHUNEK_SESSION_ID", "session-under-test")],
    )
    .await;
    assert_eq!(output.status.code(), Some(FAILURE_STATUS));
    assert!(stdout_text(&output).is_empty());
    assert!(stderr_text(&output).contains("inside a pohunek session"));
}

#[tokio::test]
async fn refused_without_a_terminal() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let output = run_detached(&harness, &["plugin", "run", PACKAGE, SETUP], &[]).await;
    assert_eq!(output.status.code(), Some(FAILURE_STATUS));
    assert!(stdout_text(&output).is_empty());
    assert!(stderr_text(&output).contains("needs a terminal on stdin and stderr"));
}

#[tokio::test]
async fn remote_host_is_refused() {
    let harness = Harness::start().await;
    let output = run_detached(
        &harness,
        &["--host", "peer.example", "plugin", "run", PACKAGE, SETUP],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(FAILURE_STATUS));
    assert!(stdout_text(&output).is_empty());
    assert!(stderr_text(&output).contains("this machine only"));
}

#[tokio::test]
async fn owner_arguments_out_of_bounds_are_usage_errors() {
    let harness = Harness::start().await;
    let too_many: Vec<String> = (0..17).map(|index| format!("a{index}")).collect();
    let too_long = "x".repeat(257);
    // A control character in an argument may carry a pasted secret; the
    // message names the position only.
    let secret_like = "pre\u{1b}[31mSECRETVALUE";
    let cases: [(Vec<&str>, &str); 3] = [
        (
            too_many.iter().map(String::as_str).collect(),
            "more than 16 arguments",
        ),
        (
            vec![too_long.as_str()],
            "argument 1 is longer than 256 bytes",
        ),
        (
            vec!["ok", secret_like],
            "argument 2 contains a control character",
        ),
    ];
    for (owner_args, expected) in cases {
        let mut arguments = vec!["plugin", "run", PACKAGE, SETUP];
        arguments.extend(owner_args);
        let output = run_detached(&harness, &arguments, &[]).await;
        assert_eq!(output.status.code(), Some(USAGE_STATUS), "{arguments:?}");
        let stderr = stderr_text(&output);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!stderr.contains("SECRETVALUE"), "{stderr}");
    }
}

#[tokio::test]
async fn invalid_setup_name_is_a_usage_error() {
    let harness = Harness::start().await;
    let output = run_detached(&harness, &["plugin", "run", PACKAGE, "Not A Name"], &[]).await;
    assert_eq!(output.status.code(), Some(USAGE_STATUS));
    assert!(stderr_text(&output).contains("setup name is not valid"));
}

#[tokio::test]
async fn profile_program_must_be_absolute() {
    let harness = Harness::start().await;
    let output = run_detached(
        &harness,
        &[
            "plugin",
            "profile",
            "create",
            "claude-acct",
            "--base",
            "claude",
            "--program",
            "relative/wrapper.sh",
        ],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(USAGE_STATUS));
    assert!(stderr_text(&output).contains("expected an absolute path"));
}

#[tokio::test]
async fn runs_the_program_with_allowlisted_environment_and_owner_arguments() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let (code, transcript) = run_on_terminal(
        &harness,
        &["plugin", "run", PACKAGE, SETUP, "add", "work", "--login"],
    );
    assert_eq!(code, 0, "{transcript}");
    assert!(
        transcript.contains("ARGS [--fixed] [add] [work] [--login]"),
        "{transcript}"
    );

    let names = reported_names(&transcript);
    for name in SETUP_ENV {
        assert!(names.contains(&name), "{name} missing: {names:?}");
    }
    for name in &names {
        assert!(
            !name.starts_with("POHUNEK_") || SETUP_ENV.contains(name),
            "{name} must not reach the setup program"
        );
    }
    assert!(!names.contains(&DECOY_ENV[0].0), "{names:?}");
    assert!(!names.contains(&DECOY_ENV[1].0), "{names:?}");
    // Nested `pohunek` calls find the same daemon through the runtime directory
    // the CLI resolved, so it must be passed on.
    assert!(names.contains(&"XDG_RUNTIME_DIR"), "{names:?}");
}

#[tokio::test]
async fn runtime_id_alias_resolves_to_the_same_package() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let (code, transcript) = run_on_terminal(&harness, &["plugin", "run", RUNTIME, SETUP]);
    assert_eq!(code, 0, "{transcript}");
    assert!(transcript.contains("ARGS [--fixed]"), "{transcript}");
}

#[tokio::test]
async fn propagates_the_program_exit_status() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let (code, transcript) =
        run_on_terminal(&harness, &["plugin", "run", PACKAGE, SETUP, EXIT_ARG]);
    assert_eq!(code, EXIT_STATUS_CODE, "{transcript}");
}

#[tokio::test]
async fn reports_a_signalled_program_as_128_plus_signal() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let (code, transcript) =
        run_on_terminal(&harness, &["plugin", "run", PACKAGE, SETUP, SIGNAL_ARG]);
    assert_eq!(code, SIGTERM_STATUS, "{transcript}");
}

#[tokio::test]
async fn undeclared_setup_entry_is_refused_by_the_daemon() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let (code, transcript) = run_on_terminal(&harness, &["plugin", "run", PACKAGE, "missing"]);
    assert_ne!(code, 0, "{transcript}");
    assert!(
        transcript.contains("declares no such setup entry"),
        "{transcript}"
    );
    assert!(!transcript.contains("ARGS"), "{transcript}");
}

#[tokio::test]
async fn inspect_and_install_review_list_the_setup_entries() {
    let harness = Harness::start().await;
    harness.install_setup_package().await;
    let output = run_detached(&harness, &["plugin", "inspect", PACKAGE], &[]).await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    let text = stdout_text(&output);
    assert!(text.contains("Setup:       accounts"), "{text}");
    assert!(text.contains("\"bin/accounts.sh\""), "{text}");
    assert!(text.contains("[\"--fixed\"]"), "{text}");

    let (code, document) = harness.json(&["plugin", "inspect", PACKAGE]).await;
    assert_eq!(code, 0, "{document}");
    let entries: &Value = &document["ok"]["setup"];
    assert_eq!(entries[0]["name"], SETUP, "{document}");
    assert_eq!(entries[0]["program"], "bin/accounts.sh", "{document}");
}
