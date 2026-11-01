//! The `pohunek` binary against a daemon of the same protocol version that
//! sends an enum value this build does not know.
//!
//! A scripted control socket answers the CLI's request with a response taken
//! from a real error, so each outcome is what an operator observes. Slices that
//! make another enum tolerant add one scenario each here.

// Rust guideline compliant 2026-10-20

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use protocol::{ErrorClass, ProtocolError, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// The error a daemon of the same version answers `notification.list` with.
fn daemon_error() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Daemon,
        "quota_exhausted",
        "the notification inbox is over its quota",
        Some("archive old notifications".to_owned()),
    )
}

/// The wire line of `error` as a newer daemon sends it, with `class` replaced
/// by a class this build does not define.
fn newer_daemon_line(id: &str, error: ProtocolError, class: &str) -> String {
    let mut response =
        serde_json::to_value(Response::err(PROTOCOL_VERSION, id, error).expect("response"))
            .expect("response JSON");
    response["err"]["class"] = json!(class);
    serde_json::to_string(&response).expect("response line")
}

/// Runs `pohunek` with `arguments` against a scripted daemon. `answer` returns
/// the result of a request, or `None` for a method the scenario does not
/// script, which gets a health result.
fn run_against_scripted_daemon(
    arguments: &[&str],
    answer: impl Fn(&Request) -> Option<Result<Value, String>> + Send + Sync + 'static,
) -> Output {
    run_against_streaming_daemon(arguments, answer, Vec::new())
}

/// Like [`run_against_scripted_daemon`], and a `subscribe` request is
/// acknowledged and followed by the `events` lines, after which the daemon ends
/// that connection.
fn run_against_streaming_daemon(
    arguments: &[&str],
    answer: impl Fn(&Request) -> Option<Result<Value, String>> + Send + Sync + 'static,
    events: Vec<String>,
) -> Output {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind daemon socket");

    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let answer = std::sync::Arc::new(answer);
    let events = std::sync::Arc::new(events);
    let server = {
        let finished = std::sync::Arc::clone(&finished);
        std::thread::spawn(move || {
            let mut connections = Vec::new();
            loop {
                let (connection, _) = listener.accept().expect("accept a CLI connection");
                if finished.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let answer = std::sync::Arc::clone(&answer);
                let events = std::sync::Arc::clone(&events);
                connections.push(std::thread::spawn(move || {
                    serve_connection(connection, &*answer, &events);
                }));
            }
            for connection in connections {
                connection.join().expect("a fixture connection ends");
            }
        })
    };

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(arguments)
        .output()
        .expect("run CLI");
    finished.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(std::os::unix::net::UnixStream::connect(&socket).expect("end the fixture daemon"));
    server.join().expect("the fixture daemon ends");
    output
}

/// Serves the requests of one CLI connection until it closes.
fn serve_connection(
    connection: std::os::unix::net::UnixStream,
    answer: &(impl Fn(&Request) -> Option<Result<Value, String>> + ?Sized),
    events: &[String],
) {
    let mut writer = connection.try_clone().expect("clone the connection");
    for line in BufReader::new(connection).lines() {
        let Ok(line) = line else { break };
        let request: Request = serde_json::from_str(&line).expect("one request line");
        if request.method() == protocol::method::SUBSCRIBE {
            let ack = serde_json::to_string(
                &Response::ok(PROTOCOL_VERSION, request.id(), json!({"subscribed": true}))
                    .expect("response"),
            )
            .expect("response line");
            writeln!(writer, "{ack}").expect("write the subscription ack");
            for event in events {
                writeln!(writer, "{event}").expect("write an event");
            }
            break;
        }
        let reply = match answer(&request) {
            Some(Ok(result)) => serde_json::to_string(
                &Response::ok(PROTOCOL_VERSION, request.id(), result).expect("response"),
            )
            .expect("response line"),
            Some(Err(line)) => line,
            None => {
                let health = json!({
                    "status": "ok",
                    "daemon_version": "0.34.2",
                    "protocol_version": PROTOCOL_VERSION.get(),
                });
                serde_json::to_string(
                    &Response::ok(PROTOCOL_VERSION, request.id(), health).expect("response"),
                )
                .expect("response line")
            }
        };
        if writeln!(writer, "{reply}").is_err() {
            break;
        }
    }
}

/// Runs `pohunek notifications list` with `extra` arguments against a scripted
/// daemon that answers the list request with an error of an unknown `class`.
fn list_against_newer_daemon(extra: &[&str], class: &str) -> Output {
    let class = class.to_owned();
    let mut arguments = vec!["notifications", "list"];
    arguments.extend_from_slice(extra);
    run_against_scripted_daemon(&arguments, move |request| {
        (request.method() == protocol::method::NOTIFICATION_LIST)
            .then(|| Err(newer_daemon_line(request.id(), daemon_error(), &class)))
    })
}

#[test]
fn an_error_class_from_a_newer_daemon_keeps_the_daemons_code_and_message() {
    let output = list_against_newer_daemon(&[], "quota");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("quota_exhausted"), "{stderr}");
    assert!(
        stderr.contains("the notification inbox is over its quota"),
        "{stderr}"
    );
    assert!(!stderr.contains("json_error"), "{stderr}");
}

#[test]
fn an_error_class_from_a_newer_daemon_stays_a_json_error_document() {
    let output = list_against_newer_daemon(&["--json"], "quota");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        document["err"],
        json!({
            "class": "unknown",
            "code": "quota_exhausted",
            "msg": "the notification inbox is over its quota",
            "recover": "archive old notifications",
        }),
        "{document}"
    );
}

/// A session snapshot of a daemon that defines a newer activity, state source,
/// working-directory source, subagent lifecycle and background kind and status.
fn newer_daemon_session() -> Value {
    json!({
        "id": "s-1",
        "agent": "shell",
        "agent_base": "shell",
        "cwd": "/workspace/project",
        "cwd_source": "a_source_from_a_newer_daemon",
        "pid": 4242,
        "cols": 120,
        "rows": 40,
        "state": "running",
        "state_source": "a_source_from_a_newer_daemon",
        "activity": "an_activity_from_a_newer_daemon",
        "subagents": [{
            "id": "sub-1",
            "provider": "codex",
            "lifecycle": "a_lifecycle_from_a_newer_daemon",
            "activity": "an_activity_from_a_newer_daemon",
            "revision": "1",
            "started_at_ms": 1,
            "updated_at_ms": 2,
        }],
        "background": {
            "revision": "1",
            "observed_at_ms": 3,
            "items": [{
                "id": "task-1",
                "kind": "a_kind_from_a_newer_daemon",
                "status": "a_status_from_a_newer_daemon",
                "first_seen_ms": 1,
                "last_seen_ms": 2,
            }],
        },
        "created_at": "2026-06-17T10:00:00Z",
        "updated_at": "2026-06-17T10:01:00Z",
    })
}

/// Answers `session.list` with one snapshot and `session.inspect` with it.
fn session_snapshot_daemon(session: Value) -> impl Fn(&Request) -> Option<Result<Value, String>> {
    move |request| match request.method() {
        protocol::method::SESSION_LIST => Some(Ok(json!([session.clone()]))),
        protocol::method::SESSION_INSPECT => Some(Ok(session.clone())),
        _ => None,
    }
}

#[test]
fn a_session_list_with_display_values_from_a_newer_daemon_shows_unknown() {
    let output = run_against_scripted_daemon(
        &["session", "list"],
        session_snapshot_daemon(newer_daemon_session()),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("s-1"), "{stdout}");
    assert!(stdout.contains("unknown"), "{stdout}");
    assert!(
        stdout.contains("0/1"),
        "a subagent of unknown lifecycle is not counted as running: {stdout}"
    );
}

#[test]
fn a_session_inspect_with_display_values_from_a_newer_daemon_reemits_unknown() {
    let output = run_against_scripted_daemon(
        &["session", "inspect", "s-1", "--json"],
        session_snapshot_daemon(newer_daemon_session()),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let session = &document["ok"];
    assert_eq!(session["activity"], "unknown", "{document}");
    assert_eq!(session["state_source"], "unknown", "{document}");
    assert_eq!(session["cwd_source"], "unknown", "{document}");
    assert_eq!(
        session["subagents"][0]["lifecycle"], "unknown",
        "{document}"
    );
    assert_eq!(session["subagents"][0]["activity"], "unknown", "{document}");
    assert_eq!(
        session["background"]["items"][0]["kind"], "other",
        "{document}"
    );
    assert_eq!(
        session["background"]["items"][0]["status"], "other",
        "{document}"
    );
}

/// Answers `method` with `result` and every other method with a health result.
fn single_result_daemon(
    method: &'static str,
    result: Value,
) -> impl Fn(&Request) -> Option<Result<Value, String>> {
    move |request| (request.method() == method).then(|| Ok(result.clone()))
}

#[test]
fn a_doctor_check_status_from_a_newer_daemon_keeps_the_daemon_checks_visible() {
    let output = run_against_scripted_daemon(
        &["doctor"],
        single_result_daemon(
            protocol::method::DAEMON_DOCTOR,
            json!({
                "report": {
                    "checks": [
                        {"name": "newer_daemon_check", "status": "a_status_from_a_newer_daemon", "detail": "judged by the daemon"},
                    ],
                    "overall": "a_status_from_a_newer_daemon",
                },
            }),
        ),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("newer_daemon_check"), "{stdout}");
    let row = stdout
        .lines()
        .find(|line| line.contains("newer_daemon_check"))
        .expect("the daemon check row");
    assert!(row.contains("unknown"), "{row}");
    assert!(
        !stdout.contains("host_governance_inspection"),
        "the daemon checks are not replaced by the unavailable check: {stdout}"
    );
}

fn newer_daemon_integration_status() -> Value {
    json!({
        "agent": "claude",
        "available": true,
        "expected_asset_paths": [],
        "present_asset_paths": [],
        "registration_paths": [],
        "expected_version": 1,
        "state": "a_state_from_a_newer_daemon",
        "recovery": "a_recovery_from_a_newer_daemon",
    })
}

#[test]
fn an_integration_status_from_a_newer_daemon_shows_unknown_and_a_generic_hint() {
    let status = json!({ "agents": [newer_daemon_integration_status()] });

    let human = run_against_scripted_daemon(
        &["integration", "status"],
        single_result_daemon(protocol::method::INTEGRATION_STATUS, status.clone()),
    );
    assert_eq!(human.status.code(), Some(0), "{human:?}");
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("unknown"), "{stdout}");
    assert!(stdout.contains("upgrade the client"), "{stdout}");

    let json_run = run_against_scripted_daemon(
        &["integration", "status", "--json"],
        single_result_daemon(protocol::method::INTEGRATION_STATUS, status),
    );
    assert_eq!(json_run.status.code(), Some(0), "{json_run:?}");
    let document: Value = serde_json::from_slice(&json_run.stdout).expect("one JSON document");
    let agent = &document["ok"]["agents"][0];
    assert_eq!(agent["state"], "unknown", "{document}");
    assert_eq!(agent["recovery"], "unknown", "{document}");
}

#[test]
fn an_integration_doctor_finding_from_a_newer_daemon_renders_unknown() {
    let doctor = json!({
        "ok": true,
        "agents": [{
            "agent": "claude",
            "ok": true,
            "status": newer_daemon_integration_status(),
            "findings": [{
                "code": "a_code_from_a_newer_daemon",
                "severity": "a_severity_from_a_newer_daemon",
                "summary": "judged by the daemon",
            }],
        }],
    });

    let human = run_against_scripted_daemon(
        &["integration", "doctor"],
        single_result_daemon(protocol::method::INTEGRATION_DOCTOR, doctor.clone()),
    );
    assert_eq!(human.status.code(), Some(0), "{human:?}");
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("[unknown] unknown: judged by the daemon"),
        "{stdout}"
    );

    let json_run = run_against_scripted_daemon(
        &["integration", "doctor", "--json"],
        single_result_daemon(protocol::method::INTEGRATION_DOCTOR, doctor),
    );
    assert_eq!(json_run.status.code(), Some(0), "{json_run:?}");
    let document: Value = serde_json::from_slice(&json_run.stdout).expect("one JSON document");
    let finding = &document["ok"]["agents"][0]["findings"][0];
    assert_eq!(finding["code"], "unknown", "{document}");
    assert_eq!(finding["severity"], "unknown", "{document}");
}

/// One `notification_created` event line whose record carries `severity` and
/// `status` as raw wire strings, a kind this build knows.
fn notification_created_line(id: &str, severity: &str, status: &str) -> String {
    notification_created_line_of_kind(id, "approval_required", severity, status)
}

/// Like [`notification_created_line`] with the `kind` as a raw wire string.
fn notification_created_line_of_kind(id: &str, kind: &str, severity: &str, status: &str) -> String {
    let record = protocol::NotificationRecord {
        id: protocol::NotificationId(id.to_owned()),
        source: protocol::NotificationSource {
            provider: "codex".to_owned(),
            provider_event: "permission_request".to_owned(),
            host_local_source_id: "src-1".to_owned(),
        },
        kind: protocol::NotificationKind::ApprovalRequired,
        severity: protocol::NotificationSeverity::Info,
        status: protocol::NotificationStatus::Unread,
        title: format!("title {id}"),
        body: String::new(),
        metadata: std::collections::BTreeMap::new(),
        created_at: "2026-07-03T00:00:00Z".to_owned(),
        session_id: None,
        agent_kind: None,
        source_id: None,
        dedupe_key: None,
        project_id: None,
        read_at: None,
        acked_at: None,
        archived_at: None,
        deleted_at: None,
        superseded_by: None,
        turn_started_at: None,
        turn_duration_ms: None,
        session_metadata: std::collections::BTreeMap::new(),
        hook_admitted: false,
    };
    let mut record = serde_json::to_value(record).expect("record JSON");
    record["kind"] = json!(kind);
    record["severity"] = json!(severity);
    record["status"] = json!(status);
    let event = protocol::Event::new(
        PROTOCOL_VERSION,
        protocol::event::NOTIFICATION_CREATED,
        json!({ "record": record }),
    )
    .expect("event");
    serde_json::to_string(&event).expect("event line")
}

/// An unknown-valued record followed by a known one, as a newer daemon streams them.
fn newer_daemon_notification_stream() -> Vec<String> {
    vec![
        notification_created_line(
            "n-newer",
            "a_severity_from_a_newer_daemon",
            "a_status_from_a_newer_daemon",
        ),
        notification_created_line("n-known", "info", "unread"),
    ]
}

fn watch_against_newer_daemon(extra: &[&str]) -> Output {
    let mut arguments = vec!["notifications", "watch"];
    arguments.extend_from_slice(extra);
    run_against_streaming_daemon(
        &arguments,
        |_request| None,
        newer_daemon_notification_stream(),
    )
}

#[test]
fn a_notification_watch_keeps_streaming_past_a_record_with_values_from_a_newer_daemon() {
    let output = watch_against_newer_daemon(&["--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let records: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON line per record"))
        .collect();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["record"]["id"], "n-newer", "{records:?}");
    assert_eq!(records[0]["record"]["severity"], "unknown", "{records:?}");
    assert_eq!(records[0]["record"]["status"], "unknown", "{records:?}");
    assert_eq!(records[1]["record"]["id"], "n-known", "{records:?}");
    assert_eq!(records[1]["record"]["severity"], "info", "{records:?}");
    assert_eq!(records[1]["record"]["status"], "unread", "{records:?}");
}

#[test]
fn a_notification_watch_filter_does_not_select_a_record_of_unknown_status() {
    for filter in [&["--unread"][..], &["--status", "unread"][..]] {
        let output = watch_against_newer_daemon(filter);

        assert_eq!(output.status.code(), Some(0), "{filter:?}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("n-newer"), "{filter:?}: {stdout}");
        assert!(stdout.contains("n-known"), "{filter:?}: {stdout}");
    }
}

/// A record of a kind this build does not know followed by a known one.
fn newer_kind_notification_stream() -> Vec<String> {
    vec![
        notification_created_line_of_kind(
            "n-newer-kind",
            "a_kind_from_a_newer_daemon",
            "info",
            "unread",
        ),
        notification_created_line("n-known", "info", "unread"),
    ]
}

fn watch_a_newer_kind(extra: &[&str]) -> Output {
    let mut arguments = vec!["notifications", "watch"];
    arguments.extend_from_slice(extra);
    run_against_streaming_daemon(
        &arguments,
        |_request| None,
        newer_kind_notification_stream(),
    )
}

#[test]
fn a_notification_watch_keeps_streaming_past_a_record_of_a_kind_from_a_newer_daemon() {
    let output = watch_a_newer_kind(&["--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let records: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON line per record"))
        .collect();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["record"]["id"], "n-newer-kind", "{records:?}");
    assert_eq!(records[0]["record"]["kind"], "unknown", "{records:?}");
    assert_eq!(records[1]["record"]["id"], "n-known", "{records:?}");
    assert_eq!(
        records[1]["record"]["kind"], "approval_required",
        "{records:?}"
    );
}

#[test]
fn a_notification_watch_kind_filter_does_not_select_a_record_of_unknown_kind() {
    let output = watch_a_newer_kind(&["--kind", "approval_required"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("n-newer-kind"), "{stdout}");
    assert!(stdout.contains("n-known"), "{stdout}");
}

/// `pohunek notifications list` against a daemon whose result holds a record
/// of a kind this build does not know followed by a known one.
fn list_a_newer_kind(extra: &[&str]) -> Output {
    let record = |id: &str, kind: &str| {
        let line = notification_created_line_of_kind(id, kind, "info", "unread");
        let event: Value = serde_json::from_str(&line).expect("event JSON");
        event["record"].clone()
    };
    let result = json!({
        "notifications": [
            record("n-newer-kind", "a_kind_from_a_newer_daemon"),
            record("n-known", "approval_required"),
        ]
    });
    let mut arguments = vec!["notifications", "list"];
    arguments.extend_from_slice(extra);
    run_against_scripted_daemon(&arguments, move |request| {
        (request.method() == protocol::method::NOTIFICATION_LIST).then(|| Ok(result.clone()))
    })
}

#[test]
fn a_notification_list_shows_a_record_of_a_kind_from_a_newer_daemon_as_unknown() {
    let output = list_a_newer_kind(&["--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let records = document["ok"]["notifications"]
        .as_array()
        .expect("notification records");
    assert_eq!(records.len(), 2, "{document}");
    let kind_of = |id: &str| {
        records
            .iter()
            .find(|record| record["id"] == id)
            .unwrap_or_else(|| panic!("{id} is listed: {document}"))["kind"]
            .clone()
    };
    assert_eq!(kind_of("n-newer-kind"), "unknown", "{document}");
    assert_eq!(kind_of("n-known"), "approval_required", "{document}");
}

fn newer_daemon_project() -> Value {
    json!({
        "id": "p-1",
        "label": "ui",
        "repo_root": "/workspace/ui",
        "git_common_dir": "/workspace/ui/.git",
        "source": "a_source_from_a_newer_daemon",
        "is_bare": false,
        "added_at": "2026-06-17T10:00:00Z",
        "last_used_at": "2026-06-17T10:01:00Z",
    })
}

#[test]
fn a_project_list_with_a_source_from_a_newer_daemon_shows_unknown() {
    let projects = json!([newer_daemon_project()]);

    let human = run_against_scripted_daemon(
        &["project", "list"],
        single_result_daemon(protocol::method::PROJECT_LIST, projects.clone()),
    );
    assert_eq!(human.status.code(), Some(0), "{human:?}");
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("unknown"),
        "{human:?}"
    );

    let json_run = run_against_scripted_daemon(
        &["project", "list", "--json"],
        single_result_daemon(protocol::method::PROJECT_LIST, projects),
    );
    assert_eq!(json_run.status.code(), Some(0), "{json_run:?}");
    let document: Value = serde_json::from_slice(&json_run.stdout).expect("one JSON document");
    assert_eq!(document["ok"][0]["source"], "unknown", "{document}");
}

#[test]
fn project_actions_and_prompts_from_a_newer_daemon_show_unknown_and_keep_none() {
    let actions = json!({
        "actions": [
            {"name": "review", "provider": "a_provider_from_a_newer_daemon", "template": "t", "layer": "a_layer_from_a_newer_daemon"},
            {"name": "plain", "provider": "none", "template": "t", "layer": "host"},
        ],
    });
    let output = run_against_scripted_daemon(
        &["project", "actions", "ui", "--json"],
        single_result_daemon(protocol::method::PROJECT_ACTIONS, actions),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let listed = &document["ok"]["actions"];
    assert_eq!(listed[0]["provider"], "unknown", "{document}");
    assert_eq!(listed[0]["layer"], "unknown", "{document}");
    assert_eq!(listed[1]["provider"], "none", "{document}");

    let action = json!({
        "provider": "a_provider_from_a_newer_daemon",
        "agent": "codex",
        "prompt_name": "review",
        "prompt_content": "Review ${pr}",
    });
    let output = run_against_scripted_daemon(
        &["project", "action", "ui", "review"],
        single_result_daemon(protocol::method::PROJECT_ACTION, action),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("provider:    unknown"),
        "{output:?}"
    );

    let prompt = json!({
        "name": "review",
        "content": "Review ${pr}",
        "layer": "a_layer_from_a_newer_daemon",
    });
    let output = run_against_scripted_daemon(
        &["project", "prompt", "ui", "review", "--json"],
        single_result_daemon(protocol::method::PROJECT_PROMPT, prompt),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["ok"]["layer"], "unknown", "{document}");
}

/// A governance status of a daemon that quarantines for a reason this build does
/// not define.
fn newer_daemon_quarantined_status() -> Value {
    let identifier = |prefix: &str| format!("{prefix}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let status = protocol::HostGovernanceStatus::new(
        protocol::HostId::parse(&identifier("host_")).expect("host id"),
        Some(protocol::EnrollmentInfo::new(
            protocol::RelayId::parse(&identifier("relay_")).expect("relay id"),
            protocol::EnrollmentStatus::Quarantined,
            protocol::EnrollmentRevision::new(2).expect("revision"),
        )),
        Some(protocol::HostOwner::Principal(
            protocol::PrincipalId::parse(&identifier("principal_")).expect("principal id"),
        )),
        Some(protocol::OwnerRevision::new(3).expect("owner revision")),
        Some(protocol::QuarantineReason::ProjectionConflict),
        protocol::ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
    )
    .expect("valid quarantined status");
    let mut status = serde_json::to_value(status).expect("status JSON");
    status["quarantine"] = json!("a_reason_from_a_newer_daemon");
    status
}

#[test]
fn a_governance_quarantine_reason_from_a_newer_daemon_shows_unknown() {
    let output = run_against_scripted_daemon(
        &["host", "governance", "inspect", "local", "--json"],
        single_result_daemon(
            protocol::method::HOST_GOVERNANCE_INSPECT,
            newer_daemon_quarantined_status(),
        ),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["ok"]["quarantine"], "unknown", "{document}");
    assert_eq!(
        document["ok"]["enrollment"]["status"], "quarantined",
        "{document}"
    );
}

#[test]
fn a_governance_quarantine_reason_from_a_newer_daemon_prints_unknown_for_humans() {
    let output = run_against_scripted_daemon(
        &["host", "governance", "inspect", "local"],
        single_result_daemon(
            protocol::method::HOST_GOVERNANCE_INSPECT,
            newer_daemon_quarantined_status(),
        ),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("quarantine:             unknown"),
        "{stdout}"
    );
}

/// A terminal session whose runtime a newer daemon reports in a state this
/// build does not know.
fn newer_daemon_runtime_session() -> Value {
    let mut session = newer_daemon_session();
    session["state"] = json!("done");
    session["runtime"] = json!({
        "state": "a_state_from_a_newer_daemon",
        "runtime_generation": "1",
    });
    session
}

#[test]
fn a_runtime_state_from_a_newer_daemon_shows_unknown_in_list_and_inspect() {
    let list = run_against_scripted_daemon(
        &["session", "list"],
        session_snapshot_daemon(newer_daemon_runtime_session()),
    );
    assert_eq!(list.status.code(), Some(0), "{list:?}");

    let inspect = run_against_scripted_daemon(
        &["session", "inspect", "s-1", "--json"],
        session_snapshot_daemon(newer_daemon_runtime_session()),
    );
    assert_eq!(inspect.status.code(), Some(0), "{inspect:?}");
    let document: Value = serde_json::from_slice(&inspect.stdout).expect("one JSON document");
    assert_eq!(document["ok"]["runtime"]["state"], "unknown", "{document}");
}

#[test]
fn stop_and_remove_refuse_a_session_with_an_unknown_runtime_state_without_a_request() {
    for (arguments, operation) in [
        (vec!["session", "stop", "s-1", "--json"], "stop"),
        (vec!["session", "rm", "s-1", "--json"], "remove"),
    ] {
        let methods = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = std::sync::Arc::clone(&methods);
        let session = newer_daemon_runtime_session();
        let output = run_against_scripted_daemon(&arguments, move |request| {
            recorded
                .lock()
                .expect("method log")
                .push(request.method().to_owned());
            match request.method() {
                protocol::method::SESSION_LIST => Some(Ok(json!([session.clone()]))),
                _ => None,
            }
        });

        assert_ne!(output.status.code(), Some(0), "{operation}: {output:?}");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("session_runtime_state_unknown"),
            "{operation}: {text}"
        );
        let methods = methods.lock().expect("method log");
        assert!(
            !methods.iter().any(|method| {
                method == protocol::method::SESSION_STOP
                    || method == protocol::method::SESSION_REMOVE
                    || method == protocol::method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED
            }),
            "{operation}: no lifecycle request is sent: {methods:?}"
        );
    }
}

/// Answers each `(method, result)` pair; every other method gets a health result.
fn results_daemon(
    results: Vec<(&'static str, Value)>,
) -> impl Fn(&Request) -> Option<Result<Value, String>> {
    move |request| {
        results
            .iter()
            .find(|(method, _)| *method == request.method())
            .map(|(_, result)| Ok(result.clone()))
    }
}

fn plain_session() -> Value {
    json!({
        "id": "s-1",
        "agent": "shell",
        "agent_base": "shell",
        "cwd": "/workspace/project",
        "pid": 4242,
        "cols": 120,
        "rows": 40,
        "state": "running",
        "state_source": "process",
        "created_at": "2026-06-17T10:00:00Z",
        "updated_at": "2026-06-17T10:01:00Z",
    })
}

#[test]
fn a_session_wait_reason_from_a_newer_daemon_exits_non_zero_with_a_defined_error() {
    let output = run_against_scripted_daemon(
        &[
            "session",
            "wait",
            "s-1",
            "--state",
            "running",
            "--timeout-ms",
            "1000",
            "--json",
        ],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_WAIT,
                json!({"reason": "a_reason_from_a_newer_daemon", "session": plain_session()}),
            ),
        ]),
    );

    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        document["err"]["code"], "session_wait_reason_unknown",
        "{document}"
    );
    assert!(document.get("ok").is_none(), "{document}");

    let human = run_against_scripted_daemon(
        &[
            "session",
            "wait",
            "s-1",
            "--state",
            "running",
            "--timeout-ms",
            "1000",
        ],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_WAIT,
                json!({"reason": "a_reason_from_a_newer_daemon", "session": plain_session()}),
            ),
        ]),
    );
    assert_ne!(human.status.code(), Some(0), "{human:?}");
    assert!(
        String::from_utf8_lossy(&human.stdout).starts_with("Unknown\t"),
        "the human run prints the result: {human:?}"
    );
    assert!(
        String::from_utf8_lossy(&human.stderr).contains("reason this client does not know"),
        "{human:?}"
    );
}

#[test]
fn a_turn_wait_reason_from_a_newer_daemon_shows_unknown_and_keeps_the_cursor() {
    let output = run_against_scripted_daemon(
        &["session", "wait", "s-1", "--json"],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_WAIT_TURN,
                json!({
                    "reason": "a_reason_from_a_newer_daemon",
                    "cursor": "cursor-after-newer-reason",
                    "progress": [],
                }),
            ),
        ]),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["ok"]["reason"], "unknown", "{document}");
    assert_eq!(
        document["ok"]["cursor"], "cursor-after-newer-reason",
        "{document}"
    );
}

#[test]
fn a_retention_hold_and_reason_from_a_newer_daemon_show_unknown_and_never_free_to_remove() {
    let result = json!({
        "dry_run": true,
        "examined": 1,
        "eligible": 0,
        "held": 1,
        "removed": 0,
        "worktrees_cleaned": 0,
        "worktrees_failed": 0,
        "failed": 0,
        "sessions": [{
            "session_id": "s-1",
            "reason": "a_reason_from_a_newer_daemon",
            "age_secs": 90,
            "hold": "a_hold_from_a_newer_daemon",
            "removed": false,
        }],
    });
    let human = run_against_scripted_daemon(
        &["session", "retention", "sweep", "--dry-run"],
        single_result_daemon(protocol::method::SESSION_RETENTION_SWEEP, result.clone()),
    );
    assert_eq!(human.status.code(), Some(0), "{human:?}");
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("reason=unknown"), "{stdout}");
    assert!(
        stdout.contains("hold=unknown"),
        "an unknown hold is never shown as free to remove (`-`): {stdout}"
    );

    let json_output = run_against_scripted_daemon(
        &["session", "retention", "sweep", "--dry-run", "--json"],
        single_result_daemon(protocol::method::SESSION_RETENTION_SWEEP, result),
    );
    let document: Value = serde_json::from_slice(&json_output.stdout).expect("one JSON document");
    assert_eq!(
        document["ok"]["sessions"][0]["hold"], "unknown",
        "{document}"
    );
}

#[test]
fn detection_regions_and_inventory_status_from_a_newer_daemon_are_dropped_or_shown_unknown() {
    let detection = run_against_scripted_daemon(
        &["session", "detection", "s-1", "--json"],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_DETECTION,
                json!({
                    "session_id": "s-1",
                    "supported_regions": ["osc_title", "a_region_from_a_newer_daemon"],
                    "previews": [{
                        "kind": "a_region_from_a_newer_daemon",
                        "region": "future_region",
                        "text": "preview",
                    }],
                }),
            ),
        ]),
    );
    assert_eq!(detection.status.code(), Some(0), "{detection:?}");
    let document: Value = serde_json::from_slice(&detection.stdout).expect("one JSON document");
    assert_eq!(
        document["ok"]["supported_regions"],
        json!(["osc_title"]),
        "{document}"
    );
    assert_eq!(
        document["ok"]["previews"][0]["kind"], "unknown",
        "{document}"
    );

    let inventory = run_against_scripted_daemon(
        &["session", "runtime-inventory", "--json"],
        single_result_daemon(
            protocol::method::SESSION_RUNTIME_INVENTORY,
            json!({"entries": [{
                "runtime_slot": "slot-1",
                "status": "a_status_from_a_newer_daemon",
            }]}),
        ),
    );
    assert_eq!(inventory.status.code(), Some(0), "{inventory:?}");
    let document: Value = serde_json::from_slice(&inventory.stdout).expect("one JSON document");
    assert_eq!(
        document["ok"]["entries"][0]["status"], "unknown",
        "{document}"
    );
}

#[test]
fn a_read_source_from_a_newer_daemon_shows_unknown() {
    let output = run_against_scripted_daemon(
        &["session", "read", "s-1", "--json"],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_READ,
                json!({
                    "text": "hello",
                    "source_used": "a_source_from_a_newer_daemon",
                    "worker_instance_id": "runtime-1",
                    "runtime_generation": "1",
                    "revision": "1",
                    "alternate_screen": false,
                    "lines_requested": 10,
                    "truncated": false,
                }),
            ),
        ]),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["ok"]["source_used"], "unknown", "{document}");

    let human = run_against_scripted_daemon(
        &["session", "read", "s-1"],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_READ,
                json!({
                    "text": "hello",
                    "source_used": "a_source_from_a_newer_daemon",
                    "worker_instance_id": "runtime-1",
                    "runtime_generation": "1",
                    "revision": "1",
                    "alternate_screen": false,
                    "lines_requested": 10,
                    "truncated": false,
                }),
            ),
        ]),
    );
    assert_eq!(human.status.code(), Some(0), "{human:?}");
}

#[test]
fn a_turn_wait_result_with_keys_from_a_newer_daemon_exits_zero_without_printing_them() {
    let output = run_against_scripted_daemon(
        &["session", "wait", "s-1", "--json"],
        results_daemon(vec![
            (protocol::method::SESSION_LIST, json!([plain_session()])),
            (
                protocol::method::SESSION_WAIT_TURN,
                json!({
                    "reason": "event",
                    "event": {
                        "session_id": "s-1",
                        "kind": "turn_completed",
                        "at": "2026-06-17T10:02:00Z",
                        "a_key_from_a_newer_daemon": 1,
                    },
                    "cursor": "cursor-after-newer-keys",
                    "progress": [{
                        "session_id": "s-1",
                        "state": "running",
                        "output_advanced": true,
                        "a_key_from_a_newer_daemon": {"nested": true},
                    }],
                    "a_key_from_a_newer_daemon": "x",
                }),
            ),
        ]),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["ok"]["reason"], "event", "{document}");
    assert_eq!(
        document["ok"]["event"]["kind"], "turn_completed",
        "{document}"
    );
    assert_eq!(
        document["ok"]["cursor"], "cursor-after-newer-keys",
        "{document}"
    );
    assert!(
        !document.to_string().contains("a_key_from_a_newer_daemon"),
        "the CLI prints only the fields it decoded: {document}"
    );
}

#[test]
fn a_session_list_with_an_unknown_capability_key_lists_every_session() {
    let mut newer = plain_session();
    newer["capabilities"] = json!({"resume": true, "fork": false, "a_key_from_a_newer_daemon": 1});
    let mut other = plain_session();
    other["id"] = json!("s-2");
    let output = run_against_scripted_daemon(
        &["session", "list", "--json"],
        results_daemon(vec![(
            protocol::method::SESSION_LIST,
            json!([newer, other]),
        )]),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let ids: Vec<&Value> = document["ok"]
        .as_array()
        .expect("a session array")
        .iter()
        .map(|session| &session["id"])
        .collect();
    assert_eq!(ids, [&json!("s-1"), &json!("s-2")], "{document}");
}

/// Answers `get_method` with `current` and records the params of `set_method`,
/// answering it with the policy it received under `field`.
fn policy_round_trip_daemon(
    get_method: &'static str,
    set_method: &'static str,
    field: &'static str,
    current: Value,
    sent: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
) -> impl Fn(&Request) -> Option<Result<Value, String>> {
    move |request| {
        if request.method() == get_method {
            return Some(Ok(json!({ field: current.clone() })));
        }
        if request.method() == set_method {
            let params = request.params().clone();
            let answer = json!({ field: params[field].clone() });
            sent.lock().expect("record the policy").push(params);
            return Some(Ok(answer));
        }
        None
    }
}

#[test]
fn a_notification_policy_set_sends_back_the_keys_of_a_newer_daemon() {
    let mut current = serde_json::to_value(protocol::NotificationPolicy {
        attention_dedupe_window_secs: 90,
        attention_debounce_secs: 7,
        enabled: protocol::NotificationKindPolicy::from_flags([
            (protocol::NotificationKind::AgentBlocked, true),
            (protocol::NotificationKind::ApprovalRequired, true),
            (protocol::NotificationKind::TurnCompleted, false),
            (protocol::NotificationKind::SessionFinished, false),
            (protocol::NotificationKind::Error, true),
            (protocol::NotificationKind::System, true),
        ]),
        providers: std::collections::BTreeMap::new(),
        retention: protocol::NotificationRetentionPolicy::default(),
        unknown_fields: protocol::tolerant::UnknownFields::default(),
    })
    .expect("policy JSON");
    current["a_key_from_a_newer_daemon"] = json!({"nested": [1, 2]});
    current["retention"]["a_retention_key_from_a_newer_daemon"] = json!(5);
    current["enabled"]["a_kind_from_a_newer_daemon"] = json!(true);
    let mut override_flags = current["enabled"].clone();
    override_flags["another_kind_from_a_newer_daemon"] = json!(false);
    current["providers"] = json!({ "codex": override_flags });
    let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let output = run_against_scripted_daemon(
        &[
            "notifications",
            "policy",
            "set",
            "--provider",
            "default",
            "--kind",
            "turn_completed",
            "--enabled",
        ],
        policy_round_trip_daemon(
            protocol::method::NOTIFICATION_POLICY_GET,
            protocol::method::NOTIFICATION_POLICY_SET,
            "policy",
            current.clone(),
            std::sync::Arc::clone(&sent),
        ),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let sent = sent.lock().expect("recorded params");
    assert_eq!(sent.len(), 1, "{sent:?}");
    let policy = &sent[0]["policy"];
    assert_eq!(policy["enabled"]["turn_completed"], true, "{policy}");
    assert_eq!(
        policy["enabled"]["a_kind_from_a_newer_daemon"], true,
        "{policy}"
    );
    assert_eq!(
        policy["providers"]["codex"], current["providers"]["codex"],
        "{policy}"
    );
    assert_eq!(
        policy["a_key_from_a_newer_daemon"], current["a_key_from_a_newer_daemon"],
        "{policy}"
    );
    assert_eq!(
        policy["retention"]["a_retention_key_from_a_newer_daemon"], 5,
        "{policy}"
    );
}

#[test]
fn a_session_policy_set_sends_back_the_retention_keys_of_a_newer_daemon() {
    let mut current =
        serde_json::to_value(protocol::SessionRetentionPolicy::default()).expect("policy JSON");
    current["a_key_from_a_newer_daemon"] = json!("kept");
    let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let output = run_against_scripted_daemon(
        &["session", "policy", "set", "--enabled"],
        policy_round_trip_daemon(
            protocol::method::SESSION_POLICY_GET,
            protocol::method::SESSION_POLICY_SET,
            "retention",
            current,
            std::sync::Arc::clone(&sent),
        ),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let sent = sent.lock().expect("recorded params");
    assert_eq!(sent.len(), 1, "{sent:?}");
    let retention = &sent[0]["retention"];
    assert_eq!(retention["enabled"], true, "{retention}");
    assert_eq!(
        retention["a_key_from_a_newer_daemon"], "kept",
        "{retention}"
    );
}
