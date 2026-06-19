//! Round-trip serialize/deserialize tests and version-negotiation tests for the
//! control protocol (milestone 1 checkpoint: "round-trip serde unit tests pass").

use std::path::PathBuf;

use protocol::{
    event, method, negotiate, AgentActivity, AgentKind, AttachHeader, ErrorClass, Event,
    IntegrationInstallParams, IntegrationInstallReport, IntegrationInstallResult, ProtocolError,
    ProtocolVersion, Request, Response, SessionAttachParams, SessionAttachResult,
    SessionDetachParams, SessionDetachResult, SessionId, SessionInfo, SessionInputParams,
    SessionInputResult, SessionNewParams, SessionReportNativeIdParams, SessionReportNativeIdResult,
    SessionResizeParams, SessionResizeResult, SessionState, SessionStopResult, SessionWarning,
    SessionWarningKind, StateSource, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

/// Serialize a value to a single JSON line, then parse it back.
fn line_roundtrip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let line = serde_json::to_string(value).expect("serialize");
    assert!(
        !line.contains('\n'),
        "wire form must be a single line (newline-delimited framing): {line}"
    );
    serde_json::from_str(&line).expect("deserialize")
}

fn running_shell_session(exit_code: Option<i32>) -> SessionInfo {
    SessionInfo {
        id: SessionId("s-42".to_owned()),
        agent: AgentKind::Shell,
        cwd: PathBuf::from("/workspace/project"),
        pid: 4242,
        cols: 120,
        rows: 40,
        state: SessionState::Running,
        state_source: StateSource::Process,
        activity: None,
        native_session_id: None,
        repo: None,
        branch: None,
        worktree_path: None,
        warnings: Vec::new(),
        created_at: "2026-06-17T10:00:00Z".to_owned(),
        updated_at: "2026-06-17T10:01:00Z".to_owned(),
        exit_code,
    }
}

#[test]
fn agent_kind_json_shape_roundtrips() {
    let cases = [
        (AgentKind::Shell, json!("shell")),
        (AgentKind::Codex, json!("codex")),
        (AgentKind::Claude, json!("claude")),
    ];

    for (agent, expected) in cases {
        let value = serde_json::to_value(agent).expect("serialize agent");
        assert_eq!(value, expected);

        let back = line_roundtrip(&agent);
        assert_eq!(back, agent);
    }
}

#[test]
fn agent_activity_json_shape_roundtrips() {
    let cases = [
        (AgentActivity::Working, json!("working")),
        (AgentActivity::Blocked, json!("blocked")),
        (AgentActivity::Idle, json!("idle")),
    ];

    for (activity, expected) in cases {
        let value = serde_json::to_value(activity).expect("serialize activity");
        assert_eq!(value, expected);

        let back = line_roundtrip(&activity);
        assert_eq!(back, activity);
    }
}

#[test]
fn session_new_params_json_shape_roundtrips() {
    let params = SessionNewParams {
        agent: AgentKind::Shell,
        cwd: Some(PathBuf::from("/workspace/project")),
        cols: 120,
        rows: 40,
        repo: None,
        branch: None,
        base_branch: None,
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(
        value,
        json!({
            "agent": "shell",
            "cwd": "/workspace/project",
            "cols": 120,
            "rows": 40
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_new_params_roundtrips_with_worktree_fields() {
    let params = SessionNewParams {
        agent: AgentKind::Claude,
        cwd: None,
        cols: 80,
        rows: 24,
        repo: Some(PathBuf::from("/workspace/project")),
        branch: Some("feature/login".to_owned()),
        base_branch: Some("main".to_owned()),
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(
        value,
        json!({
            "agent": "claude",
            "cols": 80,
            "rows": 24,
            "repo": "/workspace/project",
            "branch": "feature/login",
            "base_branch": "main"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_new_params_omits_absent_worktree_fields() {
    let params = SessionNewParams {
        agent: AgentKind::Shell,
        cwd: None,
        cols: 80,
        rows: 24,
        repo: None,
        branch: None,
        base_branch: None,
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    let object = value.as_object().expect("params object");
    for absent in ["cwd", "repo", "branch", "base_branch"] {
        assert!(
            !object.contains_key(absent),
            "absent {absent} must be omitted: {value}"
        );
    }

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_info_json_shape_roundtrips_with_activity() {
    let info = SessionInfo {
        activity: Some(AgentActivity::Working),
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(
        value,
        json!({
            "id": "s-42",
            "agent": "shell",
            "cwd": "/workspace/project",
            "pid": 4242,
            "cols": 120,
            "rows": 40,
            "state": "running",
            "state_source": "process",
            "activity": "working",
            "created_at": "2026-06-17T10:00:00Z",
            "updated_at": "2026-06-17T10:01:00Z"
        })
    );

    let back = line_roundtrip(&info);
    assert_eq!(back, info);
}

#[test]
fn session_info_json_shape_roundtrips_with_exit_code() {
    let info = SessionInfo {
        state: SessionState::Done,
        state_source: StateSource::OscTitle,
        exit_code: Some(0),
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(
        value,
        json!({
            "id": "s-42",
            "agent": "shell",
            "cwd": "/workspace/project",
            "pid": 4242,
            "cols": 120,
            "rows": 40,
            "state": "done",
            "state_source": "osc_title",
            "created_at": "2026-06-17T10:00:00Z",
            "updated_at": "2026-06-17T10:01:00Z",
            "exit_code": 0
        })
    );

    let back = line_roundtrip(&info);
    assert_eq!(back, info);
}

#[test]
fn session_info_omits_absent_activity() {
    let info = running_shell_session(None);

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert!(
        !value
            .as_object()
            .expect("session info object")
            .contains_key("activity"),
        "absent activity must be omitted: {value}"
    );

    let back = line_roundtrip(&info);
    assert_eq!(back.activity, None);
    assert_eq!(back, info);
}

#[test]
fn session_info_omits_absent_exit_code() {
    let info = running_shell_session(None);

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert!(
        !value
            .as_object()
            .expect("session info object")
            .contains_key("exit_code"),
        "absent exit_code must be omitted: {value}"
    );

    let back = line_roundtrip(&info);
    assert_eq!(back.exit_code, None);
    assert_eq!(back, info);
}

#[test]
fn session_input_method_name_is_stable() {
    assert_eq!(method::SESSION_INPUT, "session.input");
}

#[test]
fn session_input_params_json_shape_roundtrips() {
    let params = SessionInputParams {
        session_id: SessionId("s-42".to_owned()),
        text: "write tests first".to_owned(),
    };

    let value = serde_json::to_value(&params).expect("serialize input params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "text": "write tests first"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_input_result_json_shape_roundtrips() {
    for result in [
        SessionInputResult { accepted: true },
        SessionInputResult { accepted: false },
    ] {
        let value = serde_json::to_value(&result).expect("serialize input result");
        assert_eq!(value, json!({ "accepted": result.accepted }));

        let back = line_roundtrip(&result);
        assert_eq!(back, result);
    }
}

#[test]
fn session_report_native_id_method_name_is_stable() {
    assert_eq!(
        method::SESSION_REPORT_NATIVE_ID,
        "session.report_native_id"
    );
}

#[test]
fn session_report_native_id_params_roundtrips_with_transcript_path() {
    let params = SessionReportNativeIdParams {
        session_id: SessionId("s-42".to_owned()),
        agent: AgentKind::Claude,
        native_session_id: "claude-native-abc".to_owned(),
        transcript_path: Some("/home/user/.claude/transcripts/abc.jsonl".to_owned()),
    };

    let value = serde_json::to_value(&params).expect("serialize report params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "agent": "claude",
            "native_session_id": "claude-native-abc",
            "transcript_path": "/home/user/.claude/transcripts/abc.jsonl"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_report_native_id_params_omits_absent_transcript_path() {
    let params = SessionReportNativeIdParams {
        session_id: SessionId("s-7".to_owned()),
        agent: AgentKind::Codex,
        native_session_id: "codex-native-xyz".to_owned(),
        transcript_path: None,
    };

    let value = serde_json::to_value(&params).expect("serialize report params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-7",
            "agent": "codex",
            "native_session_id": "codex-native-xyz"
        })
    );
    assert!(
        !value
            .as_object()
            .expect("params object")
            .contains_key("transcript_path"),
        "absent transcript_path must be omitted: {value}"
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_report_native_id_result_roundtrips() {
    for result in [
        SessionReportNativeIdResult { recorded: true },
        SessionReportNativeIdResult { recorded: false },
    ] {
        let value = serde_json::to_value(&result).expect("serialize report result");
        assert_eq!(value, json!({ "recorded": result.recorded }));

        let back = line_roundtrip(&result);
        assert_eq!(back, result);
    }
}

#[test]
fn session_info_roundtrips_with_native_session_id() {
    let info = SessionInfo {
        native_session_id: Some("claude-native-abc".to_owned()),
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(value["native_session_id"], json!("claude-native-abc"));

    let back = line_roundtrip(&info);
    assert_eq!(back, info);
    assert_eq!(back.native_session_id.as_deref(), Some("claude-native-abc"));
}

#[test]
fn session_info_omits_absent_native_session_id() {
    let info = running_shell_session(None);

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert!(
        !value
            .as_object()
            .expect("session info object")
            .contains_key("native_session_id"),
        "absent native_session_id must be omitted: {value}"
    );

    let back = line_roundtrip(&info);
    assert_eq!(back.native_session_id, None);
}

#[test]
fn session_warning_json_shape_roundtrips() {
    let cases = [
        (SessionWarningKind::Fetch, json!("fetch")),
        (
            SessionWarningKind::BaseBranchFallback,
            json!("base_branch_fallback"),
        ),
        (SessionWarningKind::SetupScript, json!("setup_script")),
    ];
    for (kind, expected) in cases {
        let value = serde_json::to_value(kind).expect("serialize warning kind");
        assert_eq!(value, expected);
        assert_eq!(line_roundtrip(&kind), kind);
    }

    let warning = SessionWarning {
        kind: SessionWarningKind::Fetch,
        message: "Could not fetch from origin; using local base ref.".to_owned(),
        detail: Some("fatal: 'origin' does not appear to be a git repository".to_owned()),
    };
    let value = serde_json::to_value(&warning).expect("serialize warning");
    assert_eq!(
        value,
        json!({
            "kind": "fetch",
            "message": "Could not fetch from origin; using local base ref.",
            "detail": "fatal: 'origin' does not appear to be a git repository"
        })
    );
    assert_eq!(line_roundtrip(&warning), warning);
}

#[test]
fn session_warning_omits_absent_detail() {
    let warning = SessionWarning {
        kind: SessionWarningKind::SetupScript,
        message: "Repository setup script failed; the worktree was kept.".to_owned(),
        detail: None,
    };
    let value = serde_json::to_value(&warning).expect("serialize warning");
    assert!(
        !value
            .as_object()
            .expect("warning object")
            .contains_key("detail"),
        "absent detail must be omitted: {value}"
    );
    assert_eq!(line_roundtrip(&warning), warning);
}

#[test]
fn session_info_roundtrips_with_worktree_fields_and_warnings() {
    let info = SessionInfo {
        cwd: PathBuf::from("/data/worktrees/s-42-project-feature-login"),
        repo: Some(PathBuf::from("/workspace/project")),
        branch: Some("feature/login".to_owned()),
        worktree_path: Some(PathBuf::from(
            "/data/worktrees/s-42-project-feature-login",
        )),
        warnings: vec![SessionWarning {
            kind: SessionWarningKind::BaseBranchFallback,
            message: "Requested base branch \"release\" not found; used \"main\".".to_owned(),
            detail: None,
        }],
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(value["repo"], json!("/workspace/project"));
    assert_eq!(value["branch"], json!("feature/login"));
    assert_eq!(
        value["worktree_path"],
        json!("/data/worktrees/s-42-project-feature-login")
    );
    assert_eq!(value["warnings"][0]["kind"], json!("base_branch_fallback"));

    let back = line_roundtrip(&info);
    assert_eq!(back, info);
}

#[test]
fn session_info_omits_absent_worktree_fields() {
    let info = running_shell_session(None);

    let value = serde_json::to_value(&info).expect("serialize session info");
    let object = value.as_object().expect("session info object");
    for absent in ["repo", "branch", "worktree_path", "warnings"] {
        assert!(
            !object.contains_key(absent),
            "absent {absent} must be omitted: {value}"
        );
    }

    let back = line_roundtrip(&info);
    assert!(back.warnings.is_empty());
    assert_eq!(back.repo, None);
    assert_eq!(back.worktree_path, None);
}

#[test]
fn integration_install_method_name_is_stable() {
    assert_eq!(method::INTEGRATION_INSTALL, "integration.install");
}

#[test]
fn integration_install_params_roundtrips_with_and_without_agent() {
    let with_agent = IntegrationInstallParams {
        agent: Some(AgentKind::Claude),
    };
    let value = serde_json::to_value(&with_agent).expect("serialize install params");
    assert_eq!(value, json!({ "agent": "claude" }));
    assert_eq!(line_roundtrip(&with_agent), with_agent);

    let all_agents = IntegrationInstallParams { agent: None };
    let value = serde_json::to_value(&all_agents).expect("serialize install params");
    assert!(
        !value
            .as_object()
            .expect("params object")
            .contains_key("agent"),
        "absent agent selector must be omitted: {value}"
    );
    assert_eq!(line_roundtrip(&all_agents), all_agents);
}

#[test]
fn integration_install_result_roundtrips() {
    let result = IntegrationInstallResult {
        installed: vec![
            IntegrationInstallReport {
                agent: AgentKind::Claude,
                hook_path: "/home/user/.claude/hooks/zagentmesh-agent-state.sh".to_owned(),
                config_paths: vec!["/home/user/.claude/settings.json".to_owned()],
            },
            IntegrationInstallReport {
                agent: AgentKind::Codex,
                hook_path: "/home/user/.codex/zagentmesh-agent-state.sh".to_owned(),
                config_paths: vec![
                    "/home/user/.codex/hooks.json".to_owned(),
                    "/home/user/.codex/config.toml".to_owned(),
                ],
            },
        ],
    };

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
    assert_eq!(back.installed.len(), 2);
    assert_eq!(back.installed[0].agent, AgentKind::Claude);
    assert_eq!(back.installed[1].config_paths.len(), 2);
}

#[test]
fn session_stop_result_roundtrips() {
    let result = SessionStopResult { stopped: true };

    let value = serde_json::to_value(&result).expect("serialize stop result");
    assert_eq!(value, json!({ "stopped": true }));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn session_attach_params_json_shape_roundtrips() {
    let params = SessionAttachParams {
        session_id: SessionId("s-42".to_owned()),
    };

    let value = serde_json::to_value(&params).expect("serialize attach params");
    assert_eq!(value, json!({ "session_id": "s-42" }));

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_attach_result_json_shape_roundtrips() {
    let result = SessionAttachResult {
        stream_id: "stream-1".to_owned(),
    };

    let value = serde_json::to_value(&result).expect("serialize attach result");
    assert_eq!(value, json!({ "stream_id": "stream-1" }));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn session_detach_params_json_shape_roundtrips() {
    let params = SessionDetachParams {
        stream_id: "stream-1".to_owned(),
    };

    let value = serde_json::to_value(&params).expect("serialize detach params");
    assert_eq!(value, json!({ "stream_id": "stream-1" }));

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_detach_result_json_shape_roundtrips() {
    let result = SessionDetachResult { detached: true };

    let value = serde_json::to_value(&result).expect("serialize detach result");
    assert_eq!(value, json!({ "detached": true }));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn session_resize_params_json_shape_roundtrips() {
    let params = SessionResizeParams {
        session_id: SessionId("s-42".to_owned()),
        cols: 120,
        rows: 40,
    };

    let value = serde_json::to_value(&params).expect("serialize resize params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "cols": 120,
            "rows": 40
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_resize_result_carries_updated_session_info() {
    let session = running_shell_session(None);
    let result = SessionResizeResult {
        session: session.clone(),
    };

    let value = serde_json::to_value(&result).expect("serialize resize result");
    assert_eq!(
        value,
        json!({
            "session": {
                "id": "s-42",
                "agent": "shell",
                "cwd": "/workspace/project",
                "pid": 4242,
                "cols": 120,
                "rows": 40,
                "state": "running",
                "state_source": "process",
                "created_at": "2026-06-17T10:00:00Z",
                "updated_at": "2026-06-17T10:01:00Z"
            }
        })
    );

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn attach_header_json_shape_roundtrips() {
    let header = AttachHeader {
        attach: "stream-1".to_owned(),
    };

    let value = serde_json::to_value(&header).expect("serialize attach header");
    assert_eq!(value, json!({ "attach": "stream-1" }));

    let back = line_roundtrip(&header);
    assert_eq!(back, header);
}

#[test]
fn session_created_event_carries_session_info_in_flattened_payload() {
    let session = running_shell_session(None);
    let event = Event::new(event::SESSION_CREATED, json!({ "session": session }));

    let back = line_roundtrip(&event);
    assert_eq!(back, event);
    assert_eq!(back.event, event::SESSION_CREATED);

    let value = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(value["v"], json!(PROTOCOL_VERSION));
    assert_eq!(value["event"], json!("session_created"));
    assert_eq!(value["session"]["id"], json!("s-42"));
    assert_eq!(value["session"]["agent"], json!("shell"));
    assert_eq!(value["session"]["state"], json!("running"));
    assert_eq!(value["session"]["state_source"], json!("process"));
    assert!(
        !value
            .as_object()
            .expect("event object")
            .contains_key("payload"),
        "event payload fields must be flattened: {value}"
    );
}

#[test]
fn request_roundtrip() {
    let req = Request::new(
        "req-7f3",
        method::SESSION_NEW,
        json!({ "agent": "claude", "repo": "/p", "branch": "feat/x" }),
    );
    let back = line_roundtrip(&req);
    assert_eq!(req, back);
    assert_eq!(back.v, PROTOCOL_VERSION);
    assert_eq!(back.id, "req-7f3");
    assert_eq!(back.method, "session.new");
    assert_eq!(back.params["agent"], json!("claude"));
}

#[test]
fn request_missing_params_defaults_to_null() {
    // A parameterless method may omit `params` entirely on the wire.
    let raw = r#"{"v":1,"id":"req-1","method":"daemon.health"}"#;
    let req: Request = serde_json::from_str(raw).expect("deserialize");
    assert_eq!(req.method, method::DAEMON_HEALTH);
    assert_eq!(req.params, Value::Null);
}

#[test]
fn ok_response_roundtrip() {
    let resp = Response::ok(
        "req-7f3",
        json!({ "session_id": "s-42", "state": "working" }),
    );
    let back = line_roundtrip(&resp);
    assert_eq!(resp, back);
    match back {
        Response::Ok { v, id, ok } => {
            assert_eq!(v, PROTOCOL_VERSION);
            assert_eq!(id, "req-7f3");
            assert_eq!(ok["session_id"], json!("s-42"));
        }
        Response::Err { .. } => panic!("expected ok variant"),
    }
}

#[test]
fn err_response_roundtrip() {
    let err = ProtocolError::new(
        ErrorClass::Runtime,
        "agent_binary_missing",
        "claude not found on PATH",
        Some("install claude".to_owned()),
    );
    let resp = Response::err("req-7f3", err.clone());
    let back = line_roundtrip(&resp);
    assert_eq!(resp, back);
    match back {
        Response::Err { v, id, err: got } => {
            assert_eq!(v, PROTOCOL_VERSION);
            assert_eq!(id, "req-7f3");
            assert_eq!(got, err);
            assert_eq!(got.class, ErrorClass::Runtime);
            assert_eq!(got.code, "agent_binary_missing");
            assert_eq!(got.recover.as_deref(), Some("install claude"));
        }
        Response::Ok { .. } => panic!("expected err variant"),
    }
}

#[test]
fn err_response_without_recover_omits_field() {
    // `recover` is optional and must be omitted from the wire when absent.
    let resp = Response::err("req-2", ProtocolError::method_not_found("nope.method"));
    let line = serde_json::to_string(&resp).expect("serialize");
    assert!(
        !line.contains("recover"),
        "absent recover hint must not appear on the wire: {line}"
    );
    let back = line_roundtrip(&resp);
    assert_eq!(resp, back);
}

#[test]
fn agent_state_event_carries_activity_in_flattened_payload() {
    let event = Event::new(
        event::AGENT_STATE,
        json!({
            "session_id": "s-42",
            "activity": AgentActivity::Blocked,
            "source": StateSource::OscTitle
        }),
    );
    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.v, PROTOCOL_VERSION);
    assert_eq!(back.event, event::AGENT_STATE);

    let value = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(
        value,
        json!({
            "v": PROTOCOL_VERSION,
            "event": "agent_state",
            "session_id": "s-42",
            "activity": "blocked",
            "source": "osc_title"
        })
    );
    assert!(
        !value
            .as_object()
            .expect("event object")
            .contains_key("payload"),
        "event payload fields must be flattened: {value}"
    );
    assert!(
        !value
            .as_object()
            .expect("event object")
            .contains_key("state"),
        "agent activity events must not use lifecycle state key: {value}"
    );
}

#[test]
fn event_with_id_roundtrip() {
    let event = Event::new(
        "session_exit",
        json!({ "session_id": "s-7", "exit_code": 0 }),
    )
    .with_id("req-99");
    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.id.as_deref(), Some("req-99"));
}

#[test]
fn unknown_fields_are_ignored_for_additive_evolution() {
    // A newer peer may add fields; an older peer must still deserialize.
    let raw = r#"{"v":1,"id":"req-1","method":"daemon.health","params":null,"future_field":true}"#;
    let req: Request = serde_json::from_str(raw).expect("must ignore unknown fields");
    assert_eq!(req.method, method::DAEMON_HEALTH);
}

#[test]
fn negotiate_matching_versions_agrees() {
    let agreed = negotiate(PROTOCOL_VERSION, PROTOCOL_VERSION).expect("equal versions agree");
    assert_eq!(agreed, PROTOCOL_VERSION);
}

#[test]
fn negotiate_mismatched_versions_returns_typed_error() {
    let client = ProtocolVersion(1);
    let daemon = ProtocolVersion(2);
    let err = negotiate(client, daemon).expect_err("mismatch must error");
    assert_eq!(err.class, ErrorClass::Daemon);
    assert_eq!(err.code, "version_mismatch");
    assert!(err.recover.is_some(), "mismatch should suggest a recovery");
    // The error itself must round-trip as an error response body.
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn agent_binary_missing_names_binary_and_carries_recover_hint() {
    let err = ProtocolError::agent_binary_missing("claude");
    assert_eq!(err.class, ErrorClass::Runtime);
    assert_eq!(err.code, "agent_binary_missing");
    assert!(
        err.msg.contains("claude"),
        "message must name the missing binary: {}",
        err.msg
    );
    let recover = err.recover.as_deref().expect("recover hint present");
    assert!(
        recover.contains("claude"),
        "recover hint must name the binary: {recover}"
    );

    // The error round-trips as an error response body, recover hint included.
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn version_mismatch_message_names_both_versions_and_recover_hint() {
    let err = ProtocolError::version_mismatch(ProtocolVersion(1), ProtocolVersion(2));
    assert_eq!(err.code, "version_mismatch");
    // Both versions must appear so the operator sees exactly what to upgrade.
    assert!(
        err.msg.contains('1') && err.msg.contains('2'),
        "msg: {}",
        err.msg
    );
    assert!(
        err.recover
            .as_deref()
            .is_some_and(|hint| hint.contains("upgrade")),
        "recover hint must mention upgrading: {:?}",
        err.recover
    );
}

#[test]
fn protocol_version_serializes_as_bare_integer() {
    // The `v` field must be a plain integer on the wire, not an object.
    let line = serde_json::to_string(&PROTOCOL_VERSION).expect("serialize");
    assert_eq!(line, "1");
}
