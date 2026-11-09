//! The notification command group holds the attention inbox only: `list` and
//! `watch`.
//!
//! These drive the real `pohunek` binary at the argument-parsing layer, which
//! fails before any daemon connection.

use std::process::Command;

use pohunek_test_support::env::TestEnv;

thread_local! {
    /// The hermetic environment of the current test thread.
    static TEST_ENV: TestEnv = TestEnv::new().expect("create the hermetic test environment");
}

fn pohunek() -> Command {
    TEST_ENV.with(|env| env.command(pohunek_test_support::bin_exe("pohunek")))
}

/// Runs `pohunek` with `arguments` and returns its exit code and stderr.
fn run(arguments: &[&str]) -> (Option<i32>, String) {
    let output = pohunek().args(arguments).output().expect("spawn pohunek");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_record_lifecycle_policy_and_retention_subcommands_are_unknown() {
    for arguments in [
        &["notifications", "ack", "n-1"][..],
        &["notifications", "read", "n-1"],
        &["notifications", "archive", "n-1"],
        &["notifications", "delete", "n-1"],
        &["notifications", "policy", "get"],
        &["notifications", "retention", "prune", "--dry-run"],
    ] {
        let (code, stderr) = run(arguments);
        assert_eq!(code, Some(2), "{arguments:?}: {stderr}");
        assert!(
            stderr.contains("unrecognized subcommand"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn a_turn_kind_filter_is_a_usage_error_naming_the_attention_kinds() {
    for arguments in [
        &["notifications", "list", "--kind", "turn_completed"][..],
        &["notifications", "watch", "--kind", "turn_completed"],
    ] {
        let (code, stderr) = run(arguments);
        assert_eq!(code, Some(2), "{arguments:?}: {stderr}");
        assert!(
            stderr.contains("agent_blocked") && stderr.contains("approval_required"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn the_generated_completion_offers_list_and_watch_for_notifications_only() {
    let output = pohunek()
        .args(["completions", "zsh"])
        .output()
        .expect("spawn pohunek");
    assert_eq!(output.status.code(), Some(0));
    let script = String::from_utf8_lossy(&output.stdout);
    let subcommands = script
        .split("pohunek-notifications-command-$line[1]")
        .nth(1)
        .and_then(|rest| rest.split("pohunek-notifications-help-command").next())
        .expect("the notifications completion block");
    for kept in ["(list)", "(watch)"] {
        assert!(subcommands.contains(kept), "{kept} is offered");
    }
    for removed in [
        "(ack)",
        "(read)",
        "(archive)",
        "(delete)",
        "(policy)",
        "(retention)",
    ] {
        assert!(!subcommands.contains(removed), "{removed} is not offered");
    }
}
