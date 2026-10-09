//! Public CLI behavior for `pohunek assistant` argument forms.

use std::process::{Command, Output};

use pohunek_test_support::env::TestEnv;

const INTENT_WRAPPERS: [&str; 5] = ["setup", "project", "update", "debug", "help"];

thread_local! {
    static TEST_ENV: TestEnv = TestEnv::new().expect("private test environment");
}

fn pohunek() -> Command {
    TEST_ENV.with(|env| env.command(pohunek_test_support::bin_exe("pohunek")))
}

fn run_assistant(args: &[&str]) -> Output {
    pohunek().args(args).output().expect("spawn pohunek")
}

fn json_error(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("one JSON error document")
}

fn assert_reaches_daemon(args: &[&str]) {
    let output = run_assistant(args);
    let document = json_error(&output);
    assert_eq!(
        document["err"]["code"], "daemon_unreachable",
        "{args:?}: {document:?}"
    );
}

#[test]
fn assistant_flags_reach_the_daemon_on_default_and_intent_forms() {
    let project_form = [
        "--agent",
        "codex",
        "--project",
        "ui",
        "--branch",
        "feat/x",
        "--base-branch",
        "main",
        "--yes",
        "--json",
        "--print-prompt",
        "--no-snapshot",
        "--degraded",
        "--no-start-daemon",
        "--intent",
        "debug",
        "some",
        "request",
    ];
    let repo_form = [
        "--agent",
        "hermes",
        "--repo",
        "/code/ui",
        "--json",
        "--print-prompt",
        "--no-start-daemon",
        "configure",
        "it",
    ];
    for form in [&project_form[..], &repo_form[..]] {
        let args: Vec<&str> = std::iter::once("assistant")
            .chain(form.iter().copied())
            .collect();
        assert_reaches_daemon(&args);
        for wrapper in INTENT_WRAPPERS {
            let form_without_intent = form
                .iter()
                .copied()
                .filter(|arg| !matches!(*arg, "--intent" | "debug"));
            let args: Vec<&str> = ["assistant", wrapper]
                .into_iter()
                .chain(form_without_intent)
                .collect();
            assert_reaches_daemon(&args);
        }
    }
}

#[test]
fn assistant_intent_flag_accepts_every_public_value() {
    for value in INTENT_WRAPPERS {
        assert_reaches_daemon(&[
            "assistant",
            "--intent",
            value,
            "--json",
            "--no-start-daemon",
        ]);
    }
}

#[test]
fn assistant_intent_flag_rejects_unknown_value_as_usage_error() {
    let output = run_assistant(&[
        "assistant",
        "--intent",
        "nonsense",
        "--json",
        "--no-start-daemon",
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON usage error");
    assert_eq!(document["err"]["code"], "cli_usage");
    assert!(document["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("nonsense")));
}

#[test]
fn assistant_help_wrapper_is_a_command_not_the_help_screen() {
    assert_reaches_daemon(&["assistant", "help", "--json", "--no-start-daemon"]);

    let output = run_assistant(&["assistant", "--help"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(help.contains("Usage:"), "{help}");
}
