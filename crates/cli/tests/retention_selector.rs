//! The retention sweep selectors through the CLI process and the real
//! daemon: `--spawned-by` and `--session` travel into the sweep params, and
//! the sweep examines only the selected subset.

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;
use serde_json::Value;

/// Creates a shell session; `origin` is the spawning session pair.
async fn create_session(harness: &Harness, name: &str, origin: Option<(&str, &str)>) -> Value {
    let args = [
        "session", "new", "--agent", "shell", "--name", name, "--json",
    ];
    let output = match origin {
        Some((session_id, daemon_id)) => {
            harness
                .run_with_env(
                    &args,
                    &[
                        ("POHUNEK_SESSION_ID", session_id),
                        ("POHUNEK_DAEMON_ID", daemon_id),
                    ],
                )
                .await
        }
        None => harness.run(&args).await,
    };
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("session JSON envelope");
    document["ok"].clone()
}

/// Runs a dry-run sweep with `args` and returns the parsed `ok` payload.
async fn dry_sweep(harness: &Harness, args: &[&str]) -> Value {
    let mut full = vec!["session", "retention", "sweep", "--dry-run", "--json"];
    full.extend_from_slice(args);
    let output = harness.run(&full).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}/{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("sweep JSON envelope");
    document["ok"].clone()
}

#[tokio::test]
async fn sweep_spawned_by_examines_only_the_parent_children() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "sweep-parent", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    create_session(&harness, "sweep-child", Some((&parent_id, "d-sweep"))).await;
    create_session(&harness, "sweep-outsider", None).await;

    let sweep = dry_sweep(&harness, &["--spawned-by", &parent_id]).await;
    assert_eq!(sweep["examined"], 1, "{sweep}");
    assert_eq!(sweep["dry_run"], true, "{sweep}");

    // A dry run removes nothing, so the created sessions must go or their
    // workers outlive the test and trip the leaked-worker check.
    harness.stop().await;
}

#[tokio::test]
async fn sweep_session_ids_examine_only_the_named_session() {
    let harness = Harness::start().await;
    let named = create_session(&harness, "sweep-named", None).await;
    let named_id = named["id"].as_str().expect("named id").to_owned();
    create_session(&harness, "sweep-other", None).await;

    let sweep = dry_sweep(&harness, &["--session", &named_id]).await;
    assert_eq!(sweep["examined"], 1, "{sweep}");
    assert_eq!(sweep["dry_run"], true, "{sweep}");

    // A dry run removes nothing, so the created sessions must go or their
    // workers outlive the test and trip the leaked-worker check.
    harness.stop().await;
}
