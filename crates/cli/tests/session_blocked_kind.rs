//! What `pohunek session inspect` and `session list` show of a blocked agent,
//! against a scripted daemon that answers with scripted payloads.

// Rust guideline compliant 2026-11-11

#![cfg(unix)]

use std::process::Output;

use crate::newer_daemon::run_against_scripted_daemon;
use protocol::Request;
use serde_json::{json, Value};

/// A running session with the given activity and, when given, blocked kind.
fn session(id: &str, activity: Option<&str>, blocked_kind: Option<&str>) -> Value {
    let mut session = json!({
        "id": id,
        "agent": "codex",
        "agent_base": "codex",
        "cwd": "/workspace/project",
        "pid": 4242,
        "cols": 120,
        "rows": 40,
        "state": "running",
        "state_source": "osc_progress",
        "created_at": "2026-11-11T10:00:00Z",
        "updated_at": "2026-11-11T10:01:00Z",
    });
    if let Some(activity) = activity {
        session["activity"] = json!(activity);
    }
    if let Some(kind) = blocked_kind {
        session["blocked_kind"] = json!(kind);
    }
    session
}

fn daemon(sessions: Vec<Value>) -> impl Fn(&Request) -> Option<Result<Value, String>> {
    move |request| match request.method() {
        protocol::method::SESSION_LIST => Some(Ok(Value::Array(sessions.clone()))),
        protocol::method::SESSION_INSPECT => {
            let wanted = request.params().as_str().map(str::to_owned);
            sessions
                .iter()
                .find(|session| session["id"].as_str() == wanted.as_deref())
                .map(|session| Ok(session.clone()))
        }
        _ => None,
    }
}

fn stdout(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The value column of the `key` row of an inspect table.
fn inspect_value(sessions: Vec<Value>, id: &str, key: &str) -> String {
    let text = stdout(&run_against_scripted_daemon(
        &["session", "inspect", id],
        daemon(sessions),
    ));
    text.lines()
        .find_map(|line| {
            let mut columns = line.split_whitespace();
            (columns.next() == Some(key)).then(|| columns.collect::<Vec<_>>().join(" "))
        })
        .unwrap_or_else(|| panic!("no {key} row in {text}"))
}

#[test]
fn inspect_shows_what_a_blocked_agent_waits_for() {
    for kind in ["permission", "question", "auth", "unspecified"] {
        let sessions = vec![session("s-1", Some("blocked"), Some(kind))];
        assert_eq!(inspect_value(sessions, "s-1", "blocked_kind"), kind);
    }
}

#[test]
fn inspect_shows_none_when_no_kind_is_known() {
    let sessions = vec![
        session("s-blocked", Some("blocked"), None),
        session("s-idle", Some("idle"), None),
    ];
    assert_eq!(
        inspect_value(sessions.clone(), "s-blocked", "blocked_kind"),
        "<none>"
    );
    assert_eq!(inspect_value(sessions, "s-idle", "blocked_kind"), "<none>");
}

#[test]
fn inspect_prints_a_kind_a_newer_daemon_invented_as_unknown() {
    let sessions = vec![session(
        "s-1",
        Some("blocked"),
        Some("a_kind_from_a_newer_daemon"),
    )];
    assert_eq!(inspect_value(sessions, "s-1", "blocked_kind"), "unknown");
}
