//! The spawning session through the CLI process and the real daemon: a session
//! created with the origin variables of another session reports it, and
//! `session list --spawned-by` returns exactly the children.

// Rust guideline compliant 2026-11-01

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;
use serde_json::{json, Value};

/// Daemon id the scenario gives to the spawning session's runtime.
const SPAWNER_DAEMON: &str = "d-scenario-daemon";

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
    // `session.new` flattens the session into the result.
    document["ok"].clone()
}

fn listed_ids(document: &Value) -> Vec<&str> {
    let mut ids: Vec<&str> = document["ok"]
        .as_array()
        .expect("session list array")
        .iter()
        .map(|session| session["id"].as_str().expect("session id"))
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn session_list_spawned_by_returns_exactly_the_children() {
    let harness = Harness::start().await;
    let parent = create_session(&harness, "spawner", None).await;
    let parent_id = parent["id"].as_str().expect("parent id").to_owned();
    let child = create_session(&harness, "child", Some((&parent_id, SPAWNER_DAEMON))).await;
    let child_id = child["id"].as_str().expect("child id").to_owned();
    let sibling = create_session(&harness, "sibling", Some((&parent_id, SPAWNER_DAEMON))).await;
    let sibling_id = sibling["id"].as_str().expect("sibling id").to_owned();
    let stranger = create_session(
        &harness,
        "stranger",
        Some(("s-other-spawner", SPAWNER_DAEMON)),
    )
    .await;
    let stranger_id = stranger["id"].as_str().expect("stranger id").to_owned();

    assert!(
        parent.get("spawned_by").is_none(),
        "a session created outside a session reports no spawner: {parent}"
    );
    assert_eq!(
        child["spawned_by"],
        json!({ "session_id": parent_id, "daemon_id": SPAWNER_DAEMON })
    );

    let (status, inspected) = harness.json(&["session", "inspect", &child_id]).await;
    assert_eq!(status, 0, "{inspected}");
    assert_eq!(inspected["ok"]["spawned_by"]["session_id"], parent_id);

    let (status, children) = harness
        .json(&["session", "list", "--spawned-by", &parent_id])
        .await;
    assert_eq!(status, 0, "{children}");
    let mut expected = [child_id.as_str(), sibling_id.as_str()];
    expected.sort_unstable();
    assert_eq!(listed_ids(&children), expected);

    let (status, combined) = harness
        .json(&[
            "session",
            "list",
            "--spawned-by",
            &parent_id,
            "--filter",
            &format!("id={child_id}"),
        ])
        .await;
    assert_eq!(status, 0, "{combined}");
    assert_eq!(listed_ids(&combined), [child_id.as_str()]);

    let (status, other) = harness
        .json(&["session", "list", "--spawned-by", "s-other-spawner"])
        .await;
    assert_eq!(status, 0, "{other}");
    assert_eq!(listed_ids(&other), [stranger_id.as_str()]);

    let (status, none) = harness
        .json(&["session", "list", "--spawned-by", &child_id])
        .await;
    assert_eq!(status, 0, "{none}");
    assert!(listed_ids(&none).is_empty(), "{none}");

    let output = harness
        .run(&["session", "list", "--spawned-by", "", "--json"])
        .await;
    assert_ne!(
        output.status.code(),
        Some(0),
        "an empty session id is a usage error"
    );

    harness.stop().await;
}
