//! The one state `pohunek session list` and `inspect` show for a crashed
//! session, against a scripted daemon that answers with scripted payloads.
//!
//! The payloads carry only what a daemon sends; the state column, the
//! recovery outlook and the readable cause are derived by the CLI.

// Rust guideline compliant 2026-11-02

#![cfg(unix)]

use std::process::Output;

use crate::newer_daemon::run_against_scripted_daemon;
use protocol::Request;
use serde_json::{json, Value};

/// A session of the given state, with an optional runtime.
fn session(id: &str, state: &str, runtime: Option<Value>) -> Value {
    let mut session = json!({
        "id": id,
        "agent": "shell",
        "agent_base": "shell",
        "cwd": "/workspace/project",
        "pid": 4242,
        "cols": 120,
        "rows": 40,
        "state": state,
        "state_source": "process",
        "created_at": "2026-06-17T10:00:00Z",
        "updated_at": "2026-06-17T10:01:00Z",
    });
    if let Some(runtime) = runtime {
        session["runtime"] = runtime;
    }
    session
}

fn runtime(state: &str) -> Value {
    json!({"state": state, "runtime_generation": "1"})
}

fn lost(extra: &Value) -> Value {
    let mut runtime = runtime("lost");
    for (key, value) in extra.as_object().expect("runtime keys") {
        runtime[key] = value.clone();
    }
    runtime
}

/// Answers `session.list` with `sessions` and `session.inspect` with the one
/// the request names.
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

/// The text of the `STATE` column of the row of `id`.
fn state_of(table: &str, id: &str) -> String {
    let header = table.lines().next().expect("list header");
    let start = header.find("STATE").expect("STATE column");
    let row = table
        .lines()
        .find(|line| line.split_whitespace().next() == Some(id))
        .unwrap_or_else(|| panic!("row of {id} in {table}"));
    row[start..start + "reconnecting".len()].trim().to_owned()
}

fn list(sessions: Vec<Value>) -> String {
    stdout(&run_against_scripted_daemon(
        &["session", "list"],
        daemon(sessions),
    ))
}

fn inspect(sessions: Vec<Value>, id: &str) -> String {
    stdout(&run_against_scripted_daemon(
        &["session", "inspect", id],
        daemon(sessions),
    ))
}

#[test]
fn a_lost_runtime_is_one_crashed_state_whether_or_not_the_loss_is_proven() {
    let mut external = session("s-external", "running", Some(lost(&json!({}))));
    external["external"] = json!(true);
    let table = list(vec![
        session(
            "s-proven",
            "failed",
            Some(lost(&json!({"loss_reason": "runtime_lost"}))),
        ),
        session(
            "s-unproven",
            "running",
            Some(lost(
                &json!({"loss_reason": "runtime_lost_cleanup_unconfirmed"}),
            )),
        ),
        session("s-stopped-lost", "stopped", Some(lost(&json!({})))),
        external,
        session("s-failed", "failed", None),
        session("s-stopped", "stopped", Some(runtime("terminal"))),
        session("s-done", "done", Some(runtime("terminal"))),
        session("s-reconnecting", "running", Some(runtime("reconnecting"))),
        session("s-conflict", "running", Some(runtime("conflict"))),
        session("s-incompatible", "running", Some(runtime("incompatible"))),
        session("s-live", "running", Some(runtime("live"))),
        session("s-starting", "starting", Some(runtime("starting"))),
        session("s-bare", "running", None),
        session(
            "s-newer",
            "running",
            Some(runtime("a_state_from_a_newer_daemon")),
        ),
        session(
            "s-done-newer",
            "done",
            Some(runtime("a_state_from_a_newer_daemon")),
        ),
    ]);

    for (id, expected) in [
        ("s-proven", "crashed"),
        ("s-unproven", "crashed"),
        ("s-stopped-lost", "crashed"),
        ("s-external", "running"),
        ("s-failed", "failed"),
        ("s-stopped", "stopped"),
        ("s-done", "done"),
        ("s-reconnecting", "reconnecting"),
        ("s-conflict", "conflict"),
        ("s-incompatible", "incompatible"),
        ("s-live", "running"),
        ("s-starting", "starting"),
        ("s-bare", "running"),
        ("s-newer", "unknown"),
        ("s-done-newer", "done"),
    ] {
        assert_eq!(state_of(&table, id), expected, "{id}\n{table}");
    }
}

#[test]
fn a_crashed_session_says_whether_it_can_be_recovered() {
    let mut recoverable = session("s-recoverable", "failed", Some(lost(&json!({}))));
    recoverable["capabilities"] = json!({"resume": true, "fork": true});
    let mut refused = session("s-refused", "failed", Some(lost(&json!({}))));
    refused["capabilities"] = json!({"resume": true, "fork": true});
    refused["capability_refusals"] = json!({"resume": "agent_native_store_relocated"});
    let mut none = session("s-none", "failed", Some(lost(&json!({}))));
    none["capabilities"] = json!({"resume": false, "fork": false});

    let table = list(vec![recoverable, refused, none]);

    assert!(
        table.contains("lost s-recoverable: ")
            && table.contains("; recoverable with session resume"),
        "{table}"
    );
    assert!(
        table.contains("; cannot be recovered (agent_native_store_relocated)"),
        "{table}"
    );
    assert!(
        table.contains("lost s-none: ") && table.contains("; no native resume"),
        "{table}"
    );
}

#[test]
fn inspect_prints_the_readable_cause_of_every_kind_of_loss() {
    let oom = |killer: &str| {
        lost(&json!({"exit": {
            "cause": "oom_kill",
            "signal": 9,
            "oom_killer": killer,
            "memory_peak_bytes": 26_300_000_000_u64,
            "ended_at": "2026-11-01T08:39:00Z",
        }}))
    };
    let cases = [
        (
            oom("kernel"),
            "Killed by the kernel out-of-memory killer at 08:39 UTC (peak 26.3 GB)",
        ),
        (
            oom("systemd_oomd"),
            "Killed by systemd-oomd (memory pressure) at 08:39 UTC (peak 26.3 GB)",
        ),
        (
            lost(&json!({"exit": {"cause": "signal", "signal": 9}})),
            "Killed by signal 9;",
        ),
        (
            lost(&json!({"exit": {"cause": "exit_code", "code": 3}})),
            "Worker exited with code 3;",
        ),
        (
            lost(&json!({"loss_cause": "user_manager_lost"})),
            "Lost with the user service manager; see journalctl -u user@<uid>.service;",
        ),
        (
            lost(&json!({"loss_cause": "host_restarted"})),
            "Lost when the host restarted;",
        ),
        (
            lost(&json!({"exit": {"cause": "a_cause_from_a_newer_daemon"}})),
            "a_cause_from_a_newer_daemon;",
        ),
        (
            lost(&json!({"loss_cause": "a_loss_cause_from_a_newer_daemon"})),
            "a_loss_cause_from_a_newer_daemon;",
        ),
        (
            lost(&json!({"loss_reason": "runtime_lost"})),
            "The worker ended and left no record of why;",
        ),
        (
            lost(&json!({"loss_reason": "runtime_lost_cleanup_unconfirmed"})),
            "The worker ended; processes of the session may still run, check ps;",
        ),
    ];
    for (runtime, expected) in cases {
        let text = inspect(vec![session("s-1", "failed", Some(runtime.clone()))], "s-1");
        assert!(
            text.contains(&format!("lost: {expected}")),
            "{runtime}\n{text}"
        );
        assert!(
            text.lines()
                .any(|line| line.starts_with("condition") && line.ends_with("crashed")),
            "{text}"
        );
    }
}

#[test]
fn a_cause_with_terminal_control_characters_is_printed_inert() {
    let text = inspect(
        vec![session(
            "s-1",
            "failed",
            Some(lost(&json!({"loss_cause": "evil\u{1b}[31mred"}))),
        )],
        "s-1",
    );

    assert!(!text.contains('\u{1b}'), "{text:?}");
}

/// A `session.list` payload shaped like a 0.38 daemon's, written by hand
/// from the 0.38 wire type: a runtime with an `exit` of three keys, and none
/// of the keys 0.39 added.
#[test]
fn a_payload_shaped_like_a_038_daemon_still_shows_crashed() {
    let payload = session(
        "s-payload",
        "failed",
        Some(lost(&json!({
            "loss_reason": "runtime_lost",
            "worker_id": "worker-1",
            "exit": {"cause": "oom_kill", "signal": 15},
        }))),
    );

    let table = list(vec![payload.clone()]);

    assert_eq!(state_of(&table, "s-payload"), "crashed", "{table}");
    assert!(
        table.contains("lost s-payload: Killed for memory;"),
        "{table}"
    );
    let text = inspect(vec![payload], "s-payload");
    assert!(text.contains("lost: Killed for memory;"), "{text}");
}

#[test]
fn a_refusal_code_with_terminal_control_characters_is_printed_inert() {
    let mut refused = session("s-1", "failed", Some(lost(&json!({}))));
    refused["capabilities"] = json!({"resume": true, "fork": true});
    refused["capability_refusals"] = json!({"resume": "code\u{1b}[31mred"});

    let table = list(vec![refused.clone()]);
    let text = inspect(vec![refused], "s-1");

    assert!(!table.contains('\u{1b}'), "{table:?}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
}

#[test]
fn inspect_shows_the_placement_check_of_a_live_session() {
    let live = |placement: Value| {
        let mut live = session("s-1", "running", None);
        live["placement"] = placement;
        inspect(vec![live], "s-1")
    };
    let row = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("placement"))
            .map(str::to_owned)
            .unwrap_or_default()
    };

    let escaped = live(json!({
        "outcome": "escaped",
        "processes": [{"pid": 4321, "comm": "evil\u{1b}[31mcodex"}],
    }));
    let clean = live(json!({"outcome": "clean", "processes": []}));
    let unverified = live(json!({"outcome": "unobservable", "processes": []}));
    let unchecked = inspect(vec![session("s-1", "running", None)], "s-1");

    assert!(
        row(&escaped).ends_with("placement_escape: 4321 (evil\\u{1b}[31mcodex)"),
        "{escaped}"
    );
    assert!(row(&clean).ends_with("clean"), "{clean}");
    assert!(row(&unverified).ends_with("unobservable"), "{unverified}");
    assert!(row(&unchecked).ends_with("<none>"), "{unchecked}");
}
