//! `pohunek session wait` turn mode against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against a `ControlServer` served
//! from this test process with the production session registry and real
//! `shell` sessions. The turn events are `exited` events, recorded when
//! `pohunek session stop` moves a running session into a terminal state, so
//! every event a wait returns is one the daemon recorded for a real lifecycle
//! change. Each wait after the first carries a cursor issued before the stops,
//! so its result does not depend on when the wait call reaches the daemon.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{stderr_text, stdout_text, Harness};

/// Starts a named `shell` session through the CLI and returns its id.
async fn start_session(harness: &Harness, name: &str) -> String {
    let (code, created) = harness
        .json(&["session", "new", "--agent", "shell", "--name", name])
        .await;
    assert_eq!(code, 0, "{created}");
    created["ok"]["session"]["id"]
        .as_str()
        .or_else(|| created["ok"]["id"].as_str())
        .expect("created session id")
        .to_owned()
}

/// Stops `session` through the CLI.
async fn stop_session(harness: &Harness, session: &str) {
    let (code, stopped) = harness.json(&["session", "stop", session]).await;
    assert_eq!(code, 0, "{stopped}");
}

/// Runs `pohunek session wait <arguments> --json` and returns the result
/// document, asserting the documented exit status 0 for every reason.
async fn wait_json(harness: &Harness, arguments: &[&str]) -> Value {
    let mut all = vec!["session", "wait"];
    all.extend_from_slice(arguments);
    let (code, document) = harness.json(&all).await;
    assert_eq!(code, 0, "{all:?}: {document}");
    document["ok"].clone()
}

/// Runs `pohunek session wait <arguments>` in human mode and returns its
/// stdout lines, asserting exit status 0.
async fn wait_text(harness: &Harness, arguments: &[&str]) -> Vec<String> {
    let mut all = vec!["session", "wait"];
    all.extend_from_slice(arguments);
    let output = harness.run(&all).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{all:?}: {}",
        stderr_text(&output)
    );
    stdout_text(&output).lines().map(str::to_owned).collect()
}

fn cursor_of(result: &Value) -> String {
    result["cursor"].as_str().expect("result cursor").to_owned()
}

/// The `cursor:` line of a human result, which must be its last line.
fn printed_cursor(lines: &[String]) -> String {
    lines
        .last()
        .and_then(|line| line.strip_prefix("cursor: "))
        .filter(|cursor| !cursor.is_empty())
        .unwrap_or_else(|| panic!("the last line carries the cursor: {lines:?}"))
        .to_owned()
}

/// Asserts the progress entries name `sessions` in request order with `state`.
fn assert_progress(result: &Value, sessions: &[&str], states: &[&str]) {
    let progress = result["progress"].as_array().expect("progress entries");
    let named: Vec<(&str, &str)> = progress
        .iter()
        .map(|entry| {
            (
                entry["session_id"].as_str().expect("progress session"),
                entry["state"].as_str().expect("progress state"),
            )
        })
        .collect();
    let expected: Vec<(&str, &str)> = sessions
        .iter()
        .copied()
        .zip(states.iter().copied())
        .collect();
    assert_eq!(named, expected, "{result}");
    assert!(
        progress
            .iter()
            .all(|entry| entry["output_advanced"].is_boolean()),
        "{result}"
    );
}

/// A multi-session wait returns the first event after its cursor and names
/// the session; the next wait chained with the returned (or printed) cursor
/// returns the next event, not the first one again; once every target ended,
/// a wait returns `targets_ended` at once, also with no flags at all.
#[tokio::test]
async fn chained_turn_waits_return_each_event_once_and_then_targets_ended() {
    let harness = Harness::start().await;
    let first = start_session(&harness, "turn-wait-first").await;
    let second = start_session(&harness, "turn-wait-second").await;
    let targets = [first.as_str(), second.as_str()];

    let start = wait_json(&harness, &[&first, &second, "--timeout", "1ms"]).await;
    assert_eq!(start["reason"], "timeout", "{start}");
    assert!(start.get("event").is_none(), "{start}");
    assert_progress(&start, &targets, &["running", "running"]);

    stop_session(&harness, &first).await;
    stop_session(&harness, &second).await;

    let first_exit = wait_json(
        &harness,
        &[
            &first,
            &second,
            "--after",
            &cursor_of(&start),
            "--timeout",
            "5m",
        ],
    )
    .await;
    assert_eq!(first_exit["reason"], "event", "{first_exit}");
    assert_eq!(first_exit["event"]["session_id"], first.as_str());
    assert_eq!(first_exit["event"]["kind"], "exited");
    assert_progress(&first_exit, &targets, &["stopped", "stopped"]);

    // Human mode, the second target named by its display name: the event
    // line, one progress line per target, then the cursor line.
    let lines = wait_text(
        &harness,
        &[
            &first,
            "turn-wait-second",
            "--until",
            "turn_completed,exited",
            "--after",
            &cursor_of(&first_exit),
        ],
    )
    .await;
    assert_eq!(lines.len(), 4, "{lines:?}");
    let event: Vec<&str> = lines[0].split(' ').collect();
    assert_eq!(event[..2], [second.as_str(), "exited"], "{lines:?}");
    assert_eq!(
        event.len(),
        3,
        "the event line ends with its time: {lines:?}"
    );
    for (line, session) in lines[1..3].iter().zip(targets) {
        assert!(
            line.starts_with(&format!("progress: {session} stopped ")),
            "{lines:?}"
        );
        assert!(line.contains(" output_advanced="), "{lines:?}");
    }

    let ended = wait_json(
        &harness,
        &[&first, &second, "--after", &printed_cursor(&lines)],
    )
    .await;
    assert_eq!(ended["reason"], "targets_ended", "{ended}");
    assert!(ended.get("event").is_none(), "{ended}");

    // No flags: turn mode with every kind and the daemon default duration.
    let bare = wait_text(&harness, &[&first]).await;
    assert_eq!(
        bare.first().map(String::as_str),
        Some("targets_ended"),
        "{bare:?}"
    );
    assert!(
        bare[1].starts_with(&format!("progress: {first} stopped ")),
        "{bare:?}"
    );
    printed_cursor(&bare);

    harness.stop().await;
}

/// A timeout prints the progress of the target and a cursor that chains into
/// the next wait; the same target named twice is refused before the wait; a
/// legacy predicate wait on the same session keeps its result shape.
#[tokio::test]
async fn a_timeout_prints_progress_and_a_cursor_that_chains() {
    let harness = Harness::start().await;
    let session = start_session(&harness, "turn-wait-timeout").await;

    let lines = wait_text(
        &harness,
        &[&session, "--until", "turn_completed", "--timeout", "200ms"],
    )
    .await;
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0], "timeout", "{lines:?}");
    assert!(
        lines[1].starts_with(&format!("progress: {session} running ")),
        "{lines:?}"
    );
    for field in [" output_advanced=", " last_output_at=", " turn_started_at="] {
        assert!(lines[1].contains(field), "{field}: {lines:?}");
    }
    let cursor = printed_cursor(&lines);

    // The id and the display name resolve to one session.
    let duplicate = harness
        .run(&["session", "wait", &session, "turn-wait-timeout", "--json"])
        .await;
    assert_eq!(duplicate.status.code(), Some(1), "{duplicate:?}");
    let refused: Value = serde_json::from_slice(&duplicate.stdout).expect("one JSON error");
    assert_eq!(refused["err"]["code"], "cli_usage", "{refused}");
    assert!(
        refused["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("must not repeat a session")),
        "{refused}"
    );

    let legacy = wait_json(
        &harness,
        &[&session, "--state", "running", "--timeout-ms", "1000"],
    )
    .await;
    assert_eq!(legacy["reason"], "state_matched", "{legacy}");
    assert_eq!(legacy["session"]["id"], session.as_str(), "{legacy}");

    stop_session(&harness, &session).await;
    let exited = wait_json(
        &harness,
        &[
            &session,
            "--until",
            "exited",
            "--after",
            &cursor,
            "--timeout",
            "5m",
        ],
    )
    .await;
    assert_eq!(exited["reason"], "event", "{exited}");
    assert_eq!(exited["event"]["session_id"], session.as_str());
    assert_eq!(exited["event"]["kind"], "exited");

    harness.stop().await;
}
