//! Parser coverage for the `pohunek notifications` command tree.

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
            "hermes",
            "--provider",
            "hermes",
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
            "hermes",
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
fn notifications_requires_retention_mode() {
    let err = pohunek_cli::command()
        .try_get_matches_from(["pohunek", "notifications", "retention", "prune"])
        .expect_err("retention prune requires --dry-run or --apply");

    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}
