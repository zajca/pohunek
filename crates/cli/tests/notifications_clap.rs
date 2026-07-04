//! Parser coverage for the `pohunek notifications` command tree.

use std::process::Command;

fn pohunek() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pohunek"))
}

#[test]
fn notifications_subcommands_parse() {
    for args in [
        vec![
            "pohunek",
            "notifications",
            "list",
            "--all-hosts",
            "--unread",
            "--kind",
            "approval_required",
            "--severity",
            "action_required",
            "--agent",
            "codex",
            "--provider",
            "codex",
            "--session",
            "s-1",
            "--limit",
            "25",
            "--cursor",
            "next",
            "--json",
        ],
        vec!["pohunek", "notifications", "watch", "--all-hosts", "--json"],
        vec!["pohunek", "notifications", "read", "host-b/n-1", "--json"],
        vec!["pohunek", "notifications", "ack", "n-1", "--json"],
        vec!["pohunek", "notifications", "archive", "n-1", "--json"],
        vec!["pohunek", "notifications", "delete", "n-1", "--json"],
        vec![
            "pohunek",
            "notifications",
            "policy",
            "get",
            "--all-hosts",
            "--json",
        ],
        vec![
            "pohunek",
            "notifications",
            "policy",
            "set",
            "--provider",
            "codex",
            "--kind",
            "turn_completed",
            "--enabled",
            "--json",
        ],
        vec![
            "pohunek",
            "notifications",
            "policy",
            "set",
            "--provider",
            "codex",
            "--kind",
            "turn_completed",
            "--disabled",
            "--json",
        ],
        vec![
            "pohunek",
            "notifications",
            "retention",
            "prune",
            "--dry-run",
            "--status",
            "archived",
            "--before",
            "2026-07-03T10:00:00Z",
            "--limit",
            "5",
            "--json",
        ],
        vec![
            "pohunek",
            "notifications",
            "retention",
            "prune",
            "--apply",
            "--all-hosts",
            "--json",
        ],
    ] {
        pohunek_cli::command()
            .try_get_matches_from(args)
            .expect("notifications command should parse");
    }
}

#[test]
fn notifications_rejects_host_and_all_hosts() {
    let out = pohunek()
        .args([
            "--host",
            "host-b",
            "notifications",
            "list",
            "--all-hosts",
            "--json",
        ])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout must be JSON ({err}): {stdout:?}"));
    assert_eq!(doc["code"], "cli_usage");
    assert!(
        doc["msg"]
            .as_str()
            .is_some_and(|msg| { msg.contains("--host") && msg.contains("--all-hosts") }),
        "usage message should name both arguments: {doc:?}"
    );
}

#[test]
fn notifications_command_tree_accepts_host_all_hosts_for_typed_usage_validation() {
    pohunek_cli::command()
        .try_get_matches_from([
            "pohunek",
            "--host",
            "host-b",
            "notifications",
            "list",
            "--all-hosts",
        ])
        .expect("typed run_cli validation handles this global/subcommand combination");
}

#[test]
fn notifications_requires_retention_mode() {
    let err = pohunek_cli::command()
        .try_get_matches_from(["pohunek", "notifications", "retention", "prune"])
        .expect_err("retention prune requires --dry-run or --apply");

    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}
