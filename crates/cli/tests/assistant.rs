//! Public CLI behavior for `pohunek assistant` argument forms.

use std::os::unix::fs::PermissionsExt as _;
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
        "--from-remote",
        "--reset-diverged",
        "--yes",
        "--json",
        "--print-prompt",
        "--no-snapshot",
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

#[test]
fn assistant_branch_source_flags_that_contradict_each_other_are_usage_errors() {
    for args in [
        vec!["assistant", "--repo", "/code/ui", "--from-remote"],
        vec![
            "assistant",
            "--repo",
            "/code/ui",
            "--branch",
            "feat",
            "--from-remote",
            "--base-branch",
            "main",
        ],
        vec![
            "assistant",
            "--repo",
            "/code/ui",
            "--branch",
            "feat",
            "--reset-diverged",
        ],
    ] {
        let mut full = args.clone();
        full.extend(["--json", "--no-start-daemon"]);
        let output = run_assistant(&full);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn assistant_accepts_a_request_id_and_refuses_a_malformed_one_before_dialing() {
    assert_reaches_daemon(&[
        "assistant",
        "--request-id",
        "launch-1",
        "--json",
        "--no-start-daemon",
    ]);

    let too_long = "k".repeat(129);
    for key in ["", "with space", "a/b", too_long.as_str()] {
        let output = run_assistant(&[
            "assistant",
            "--request-id",
            key,
            "--json",
            "--no-start-daemon",
        ]);
        assert_eq!(output.status.code(), Some(2), "{key:?}: {output:?}");
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("one JSON usage error");
        assert_eq!(document["err"]["code"], "cli_usage", "{key:?}: {document}");
    }
}

#[test]
fn assistant_without_a_started_daemon_still_ensures_a_private_workdir() {
    // The hermetic environment holds no daemon and `--no-start-daemon` skips
    // the bootstrap, so the CLI reports `daemon_unreachable`. The launcher
    // still ensures `<data_home>/pohunek/assistant` first: without an
    // owner-private directory the session would start in the wrong place
    // once the daemon answers.
    let output = run_assistant(&["assistant", "--json", "--no-start-daemon"]);
    let document = json_error(&output);
    assert_eq!(
        document["err"]["code"], "daemon_unreachable",
        "{document:?}"
    );
    TEST_ENV.with(|env| {
        let workdir = env.data_home().join("pohunek").join("assistant");
        let metadata = std::fs::symlink_metadata(&workdir).expect("the launcher creates it");
        assert!(metadata.is_dir(), "{workdir:?}");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700, "{workdir:?}");
        std::fs::remove_dir(&workdir).expect("restore the hermetic home");
    });
}

#[test]
fn assistant_refuses_a_foreign_workdir_without_touching_it() {
    TEST_ENV.with(|env| {
        let workdir = env.data_home().join("pohunek").join("assistant");
        // A symlink where the directory must be: the launcher inspects with
        // `symlink_metadata`, refuses with the typed error, and never follows
        // the link (the target must not exist afterwards).
        std::fs::create_dir_all(workdir.parent().expect("the data dir parent"))
            .expect("create the data dir parent");
        let target = env.data_home().join("foreign-target");
        std::os::unix::fs::symlink(&target, &workdir).expect("plant the symlink");
        let output = TEST_ENV.with(|nested| {
            let mut command = nested.command(pohunek_test_support::bin_exe("pohunek"));
            command
                .args(["assistant", "--json", "--print-prompt"])
                .output()
                .expect("spawn pohunek")
        });
        let document = json_error(&output);
        assert_eq!(
            document["err"]["code"], "assistant_workdir_unusable",
            "{document:?}"
        );
        assert!(!target.exists(), "{target:?}");
        std::fs::remove_file(&workdir).expect("clear the symlink");

        // A group-writable directory the launcher did not create stays as it
        // is: refused, never chmodded.
        std::fs::create_dir(&workdir).expect("plant the directory");
        std::fs::set_permissions(&workdir, std::fs::Permissions::from_mode(0o770))
            .expect("make it group-writable");
        let output = TEST_ENV.with(|nested| {
            let mut command = nested.command(pohunek_test_support::bin_exe("pohunek"));
            command
                .args(["assistant", "--json", "--print-prompt"])
                .output()
                .expect("spawn pohunek")
        });
        let document = json_error(&output);
        assert_eq!(
            document["err"]["code"], "assistant_workdir_unusable",
            "{document:?}"
        );
        let mode = std::fs::symlink_metadata(&workdir)
            .expect("still there")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o770, "the launcher never chmods a foreign path");
        std::fs::remove_dir(&workdir).expect("clear the directory");

        // A plain file is not a directory either: the same typed error.
        std::fs::write(&workdir, b"not a directory").expect("plant the file");
        let output = TEST_ENV.with(|nested| {
            let mut command = nested.command(pohunek_test_support::bin_exe("pohunek"));
            command
                .args(["assistant", "--json", "--print-prompt"])
                .output()
                .expect("spawn pohunek")
        });
        let document = json_error(&output);
        assert_eq!(
            document["err"]["code"], "assistant_workdir_unusable",
            "{document:?}"
        );
        assert!(
            std::fs::symlink_metadata(&workdir)
                .expect("still there")
                .is_file(),
            "the launcher never replaces a foreign path"
        );
    });
}
