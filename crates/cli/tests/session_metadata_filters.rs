//! Exercise metadata list filters through the CLI process and the real daemon.

#![cfg(unix)]

use std::collections::BTreeMap;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;
use protocol::{method, SessionId, SessionSetMetadataParams};
use serde_json::Value;

async fn create_session(harness: &Harness, name: &str, metadata: &[&str]) -> String {
    let mut args = vec!["session", "new", "--agent", "shell", "--name", name];
    for pair in metadata {
        args.extend(["--meta", pair]);
    }
    args.push("--json");
    let output = harness.run(&args).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("session JSON envelope");
    document["ok"]["session"]["id"]
        .as_str()
        .or_else(|| document["ok"]["id"].as_str())
        .expect("created session id")
        .to_owned()
}

async fn set_empty_metadata_key(harness: &Harness, session_id: &str) {
    let socket = harness.env.runtime_dir().join("pohunek/daemon.sock");
    let mut client = pohunek_client::Client::connect_local(socket)
        .await
        .expect("connect to the real daemon");
    client
        .call::<method::SessionSetMetadata>(SessionSetMetadataParams {
            session_id: SessionId(session_id.to_owned()),
            metadata: BTreeMap::from([(String::new(), Some("value".to_owned()))]),
        })
        .await
        .expect("daemon accepts an empty metadata key");
}

fn listed_ids(document: &Value) -> Vec<&str> {
    document["ok"]
        .as_array()
        .expect("session list array")
        .iter()
        .map(|session| session["id"].as_str().expect("session id"))
        .collect()
}

#[tokio::test]
async fn session_list_metadata_filters_match_opaque_values_and_combine_with_and() {
    let harness = Harness::start().await;
    let first = create_session(
        &harness,
        "metadata-first",
        &[
            "link.id=owner/repo#123",
            "link.url=https://example.test/?a=b",
            "empty=",
        ],
    )
    .await;
    set_empty_metadata_key(&harness, &first).await;
    let second = create_session(
        &harness,
        "metadata-second",
        &["link.id=owner/repo#124", "link.provider=github"],
    )
    .await;
    let plain = create_session(&harness, "metadata-plain", &[]).await;

    let (status, exact) = harness
        .json(&["session", "list", "--meta-eq", "link.id=owner/repo#123"])
        .await;
    assert_eq!(status, 0, "{exact}");
    assert_eq!(listed_ids(&exact), [first.as_str()]);

    let (status, presence) = harness
        .json(&["session", "list", "--meta-has", "link.provider"])
        .await;
    assert_eq!(status, 0, "{presence}");
    assert_eq!(listed_ids(&presence), [second.as_str()]);

    let (status, repeated) = harness
        .json(&[
            "session",
            "list",
            "--meta-has",
            "link.id",
            "--meta-eq",
            "link.id=owner/repo#123",
            "--meta-eq",
            "link.url=https://example.test/?a=b",
            "--filter",
            &format!("id={first}"),
        ])
        .await;
    assert_eq!(status, 0, "{repeated}");
    assert_eq!(listed_ids(&repeated), [first.as_str()]);
    assert_eq!(repeated["ok"][0]["metadata"]["empty"], "");

    let (status, contradictory) = harness
        .json(&[
            "session",
            "list",
            "--meta-eq",
            "link.id=owner/repo#123",
            "--meta-eq",
            "link.id=owner/repo#124",
        ])
        .await;
    assert_eq!(status, 0, "{contradictory}");
    assert!(listed_ids(&contradictory).is_empty());

    let (status, empty_value) = harness
        .json(&["session", "list", "--meta-eq", "empty="])
        .await;
    assert_eq!(status, 0, "{empty_value}");
    assert_eq!(listed_ids(&empty_value), [first.as_str()]);

    let (status, empty_key) = harness
        .json(&["session", "list", "--meta-eq", "=value"])
        .await;
    assert_eq!(status, 0, "{empty_key}");
    assert_eq!(listed_ids(&empty_key), [first.as_str()]);
    let (status, empty_presence_key) = harness.json(&["session", "list", "--meta-has", ""]).await;
    assert_eq!(status, 0, "{empty_presence_key}");
    assert_eq!(listed_ids(&empty_presence_key), [first.as_str()]);

    assert_ne!(first, second);
    assert_ne!(first, plain);
    assert_ne!(second, plain);

    harness.stop().await;
}

#[tokio::test]
async fn session_list_metadata_filters_keep_table_and_quiet_output_shapes() {
    let harness = Harness::start().await;
    let first = create_session(&harness, "metadata-table-first", &["link.id=one"]).await;
    let second = create_session(&harness, "metadata-table-second", &["link.id=two"]).await;

    let table = harness
        .run(&["session", "list", "--meta-has", "link.id"])
        .await;
    assert_eq!(
        table.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&table.stdout)
    );
    let table_text = String::from_utf8_lossy(&table.stdout);
    assert!(table_text.contains(&first), "{table_text}");
    assert!(table_text.contains(&second), "{table_text}");
    assert!(!table_text.contains("link.id"), "{table_text}");

    let quiet = harness
        .run(&["session", "list", "--quiet", "--meta-eq", "link.id=one"])
        .await;
    assert_eq!(quiet.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&quiet.stdout).trim(), first);

    let quiet_empty = harness
        .run(&["session", "list", "--quiet", "--meta-has", "absent"])
        .await;
    assert_eq!(quiet_empty.status.code(), Some(0));
    assert!(quiet_empty.stdout.is_empty());
    harness.stop().await;
}

#[tokio::test]
async fn session_list_metadata_filter_limits_are_enforced_by_the_daemon() {
    let harness = Harness::start().await;
    let oversized_key = "k".repeat(65);
    let (status, oversized_key_error) = harness
        .json(&["session", "list", "--meta-has", &oversized_key])
        .await;
    assert_ne!(status, 0, "{oversized_key_error}");
    assert!(
        oversized_key_error["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("metadata key exceeds 64 bytes")),
        "{oversized_key_error}"
    );

    let oversized_value = format!("key={}", "v".repeat(4097));
    let (status, oversized_value_error) = harness
        .json(&["session", "list", "--meta-eq", &oversized_value])
        .await;
    assert_ne!(status, 0, "{oversized_value_error}");
    assert!(
        oversized_value_error["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("exceeds 4096 bytes")),
        "{oversized_value_error}"
    );
    harness.stop().await;
}
