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
    answer: impl Fn(&Request) -> Option<Result<Value, String>> + Send + 'static,
) -> Output {
    run_against_streaming_daemon(arguments, answer, Vec::new())
}

/// Like [`run_against_scripted_daemon`], and a `subscribe` request is
/// acknowledged and followed by the `events` lines, after which the daemon ends
/// that connection.
fn run_against_streaming_daemon(
    arguments: &[&str],
    answer: impl Fn(&Request) -> Option<Result<Value, String>> + Send + 'static,
    events: Vec<String>,
) -> Output {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind daemon socket");

    let server = std::thread::spawn(move || {
        loop {
            let (connection, _) = listener.accept().expect("accept a CLI connection");
            let mut writer = connection.try_clone().expect("clone the connection");
            let mut served = false;
            for line in BufReader::new(connection).lines() {
                let Ok(line) = line else { break };
                served = true;
                let request: Request = serde_json::from_str(&line).expect("one request line");
                if request.method() == protocol::method::SUBSCRIBE {
                    let ack = serde_json::to_string(
                        &Response::ok(PROTOCOL_VERSION, request.id(), json!({"subscribed": true}))
                            .expect("response"),
                    )
                    .expect("response line");
                    writeln!(writer, "{ack}").expect("write the subscription ack");
                    for event in &events {
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
                            &Response::ok(PROTOCOL_VERSION, request.id(), health)
                                .expect("response"),
                        )
                        .expect("response line")
                    }
                };
                writeln!(writer, "{reply}").expect("write the response");
            }
            // A connection without a request ends the fixture daemon.
            if !served {
                break;
            }
        }
    });

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(arguments)
        .output()
        .expect("run CLI");
    drop(std::os::unix::net::UnixStream::connect(&socket).expect("end the fixture daemon"));
    server.join().expect("the fixture daemon ends");
    output
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
/// `status` as raw wire strings.
fn notification_created_line(id: &str, severity: &str, status: &str) -> String {
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
    };
    let mut record = serde_json::to_value(record).expect("record JSON");
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
