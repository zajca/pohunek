//! Parser coverage for `pohunek session policy` and `pohunek session retention`.

// Rust guideline compliant 2026-09-22

#[test]
fn session_policy_and_retention_subcommands_parse() {
    for args in [
        vec!["pohunek", "session", "policy", "get", "--json"],
        vec!["pohunek", "session", "policy", "set", "--enabled", "--json"],
        vec!["pohunek", "session", "policy", "set", "--disabled"],
        vec![
            "pohunek",
            "session",
            "policy",
            "set",
            "--sweep-interval-secs",
            "3600",
            "--terminal-ttl-secs",
            "604800",
            "--lost-ttl-secs",
            "2592000",
            "--max-removals-per-sweep",
            "10",
            "--json",
        ],
        vec![
            "pohunek",
            "session",
            "retention",
            "sweep",
            "--dry-run",
            "--json",
        ],
        vec![
            "pohunek",
            "session",
            "retention",
            "sweep",
            "--apply",
            "--limit",
            "5",
        ],
        vec![
            "pohunek",
            "--host",
            "host-b",
            "session",
            "retention",
            "sweep",
            "--dry-run",
        ],
    ] {
        pohunek_cli::command()
            .try_get_matches_from(args)
            .expect("session policy/retention command should parse");
    }
}

#[test]
fn session_policy_set_requires_at_least_one_field() {
    pohunek_cli::command()
        .try_get_matches_from(["pohunek", "session", "policy", "set"])
        .expect_err("an empty policy update must be rejected");
}

#[test]
fn session_policy_set_rejects_enabled_and_disabled_together() {
    pohunek_cli::command()
        .try_get_matches_from([
            "pohunek",
            "session",
            "policy",
            "set",
            "--enabled",
            "--disabled",
        ])
        .expect_err("contradictory enable flags must be rejected");
}

#[test]
fn session_retention_sweep_requires_an_explicit_mode() {
    pohunek_cli::command()
        .try_get_matches_from(["pohunek", "session", "retention", "sweep"])
        .expect_err("a destructive sweep must never be the implicit default");
    pohunek_cli::command()
        .try_get_matches_from([
            "pohunek",
            "session",
            "retention",
            "sweep",
            "--dry-run",
            "--apply",
        ])
        .expect_err("dry-run and apply are mutually exclusive");
}
