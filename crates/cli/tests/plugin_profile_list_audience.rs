//! Presentation labels of `pohunek plugin profile list` (`#2150` rows 10-11)
//! and the `session list` AGENT column (row 12).
//!
//! The profiles are plain `shell`-base files written through the harness, so
//! the daemon never needs a package: the CLI reads the files itself and the
//! session assertions run against the live daemon.

// Rust guideline compliant 2026-11-13

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// A profile file: the shell base launches `/bin/sh` through the harness.
fn shell_profile(extra: &str) -> String {
    format!("base = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n{extra}")
}

/// Four profiles covering every default-list case: absent and `owner` show,
/// `agent` and `hidden` hide until `--all`.
#[tokio::test]
async fn list_shows_owner_profiles_and_hides_agent_and_hidden_until_all() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile("absent", &shell_profile(""));
    harness.profile(
        "labelled",
        &shell_profile("display_name = \"Night Shift\"\naudience = \"owner\"\n"),
    );
    harness.profile("agent", &shell_profile("audience = \"agent\"\n"));
    harness.profile("hidden", &shell_profile("audience = \"hidden\"\n"));

    let (code, document) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{document}");
    let shown: Vec<&str> = document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    assert_eq!(shown, ["absent", "labelled"], "{document}");
    assert_eq!(document["ok"]["hidden"], 2, "{document}");

    let (all_code, all) = harness.json(&["plugin", "profile", "list", "--all"]).await;
    assert_eq!(all_code, 0, "{all}");
    let all_shown: Vec<&str> = all["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        all_shown,
        ["absent", "agent", "hidden", "labelled"],
        "{all}"
    );
    assert_eq!(all["ok"]["hidden"], 0, "{all}");

    // JSON carries the labels when the file sets them.
    let labelled = all["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "labelled")
        .expect("the labelled profile");
    assert_eq!(labelled["display_name"], "Night Shift", "{all}");
    assert_eq!(labelled["audience"], "owner", "{all}");
    let absent = all["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "absent")
        .expect("the plain profile");
    assert!(absent.get("display_name").is_none(), "{all}");
    assert!(absent.get("audience").is_none(), "{all}");
    harness.stop().await;
}

/// The human table shows the footer and the `DISPLAY NAME` values.
#[tokio::test]
async fn list_table_shows_display_names_and_the_hidden_footer() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile(
        "labelled",
        &shell_profile("display_name = \"Night Shift\"\naudience = \"owner\"\n"),
    );
    harness.profile("hidden", &shell_profile("audience = \"hidden\"\n"));

    let listed = harness.run(&["plugin", "profile", "list"]).await;
    assert!(listed.status.success(), "{listed:?}");
    let table = String::from_utf8(listed.stdout).expect("table text");
    assert!(table.contains("DISPLAY NAME"), "{table}");
    assert!(table.contains("Night Shift"), "{table}");
    assert!(
        table.contains("1 profiles hidden (agent/hidden audience); use --all"),
        "{table}"
    );
    // A row starts at the line start with the profile name; the `hidden`
    // profile must leave no such row, while the footer still counts it.
    assert!(
        !table.lines().any(|line| line.starts_with("hidden ")),
        "{table}"
    );

    let all = harness.run(&["plugin", "profile", "list", "--all"]).await;
    let all_table = String::from_utf8(all.stdout).expect("table text");
    assert!(all_table.contains("hidden"), "{all_table}");
    assert!(!all_table.contains("profiles hidden"), "{all_table}");
    harness.stop().await;
}

/// An invalid audience makes the entry `unreadable` with the fixed detail,
/// and it still shows without `--all`: a broken file is never hidden. The
/// non-string case shows the same fixed daemon text, never the value.
#[tokio::test]
async fn list_shows_an_invalid_audience_as_unreadable() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile("broken", &shell_profile("audience = \"public\"\n"));

    let (code, document) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{document}");
    let entry = document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "broken")
        .expect("the broken profile shows: {document}");
    assert_eq!(entry["state"], "unreadable", "{document}");
    assert_eq!(
        entry["detail"], "audience must be one of owner, agent, hidden",
        "{document}"
    );
    harness.stop().await;
}

#[tokio::test]
async fn list_shows_a_non_string_audience_as_unreadable() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile("non-string", &shell_profile("audience = 5\n"));

    let (code, document) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{document}");
    let entry = document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "non-string")
        .expect("the non-string profile shows: {document}");
    assert_eq!(entry["state"], "unreadable", "{document}");
    assert_eq!(
        entry["detail"], "audience must be one of owner, agent, hidden",
        "{document}"
    );
    harness.stop().await;
}

/// An empty `display_name` makes the entry `unreadable` with the daemon's
/// fixed text, and it still shows without `--all`: a broken file is never
/// hidden.
#[tokio::test]
async fn list_shows_an_empty_display_name_as_unreadable() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile("empty", &shell_profile("display_name = \"   \"\n"));

    let (code, document) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{document}");
    let entry = document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "empty")
        .expect("the empty profile shows: {document}");
    assert_eq!(entry["state"], "unreadable", "{document}");
    assert_eq!(
        entry["detail"], "display_name must not be empty",
        "{document}"
    );
    harness.stop().await;
}

/// A host whose profiles are all `agent`/`hidden` hides every row but still
/// prints the hidden footer instead of "no profiles".
#[tokio::test]
async fn list_with_every_profile_hidden_still_shows_the_footer() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile("agent", &shell_profile("audience = \"agent\"\n"));
    harness.profile("hidden", &shell_profile("audience = \"hidden\"\n"));

    let listed = harness.run(&["plugin", "profile", "list"]).await;
    assert!(listed.status.success(), "{listed:?}");
    let table = String::from_utf8(listed.stdout).expect("table text");
    assert!(
        table.contains("2 profiles hidden (agent/hidden audience); use --all"),
        "{table}"
    );
    harness.stop().await;
}

/// `session list` shows `<agent> (<display_name>)` in the AGENT column.
#[tokio::test]
async fn session_list_shows_the_agent_display_name() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    harness.profile("night", &shell_profile("display_name = \"Night Shift\"\n"));
    let cwd = harness.env.cwd().to_path_buf();
    let (code, created) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            "night",
            "--cwd",
            plugin_harness::path_str(&cwd),
        ])
        .await;
    assert_eq!(code, 0, "{created}");

    let listed = harness.run(&["session", "list"]).await;
    assert!(listed.status.success(), "{listed:?}");
    let table = String::from_utf8(listed.stdout).expect("table text");
    assert!(table.contains("night (Night Shift)"), "{table}");
    harness.stop().await;
}
