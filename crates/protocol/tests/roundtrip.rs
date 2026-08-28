//! Round-trip serialize/deserialize tests and version-negotiation tests for the
//! control protocol (milestone 1 checkpoint: "round-trip serde unit tests pass").

use std::{collections::BTreeMap, path::PathBuf};

use protocol::{
    event, method, negotiate, AgentActivity, AgentKind, AgentRuntime, AssistantMaterializeParams,
    AssistantMaterializeResult, AttachHeader, ConceptDeprecation, ConceptIntent, ConceptMeta,
    ConceptType, CwdSource, DaemonDoctorResult, DetectionRegionKind, DetectionRegionPreview,
    DoctorCheck, DoctorReport, DoctorStatus, ErrorClass, Event, ForkCwdMode, HostCapabilities,
    IntegrationInstallParams, IntegrationInstallReport, IntegrationInstallResult,
    NotificationCreateParams, NotificationCreateResult, NotificationCreatedEvent,
    NotificationDeleteParams, NotificationDeleteResult, NotificationDeletedEvent, NotificationId,
    NotificationKind, NotificationKindPolicy, NotificationListParams, NotificationListResult,
    NotificationPolicy, NotificationPolicyParams, NotificationPolicyResult, NotificationRecord,
    NotificationRetentionParams, NotificationRetentionPolicy, NotificationRetentionResult,
    NotificationSeverity, NotificationSource, NotificationStatus, NotificationUpdateParams,
    NotificationUpdateResult, NotificationUpdatedEvent, ObservationParamsError, OutputOffset,
    ProcessStartIdentity, ProjectSource, ProtocolError, ProtocolVersion, ProtocolVersionRange,
    ProviderKind, ReportSequence, Request, Response, RuntimeGeneration, SessionAttachParams,
    SessionAttachResult, SessionCapabilities, SessionDetachParams, SessionDetachResult,
    SessionDetectionParams, SessionDetectionResult, SessionForkParams, SessionForkResult,
    SessionId, SessionInfo, SessionInputParams, SessionInputResult, SessionListFilter,
    SessionListParams, SessionNewParams, SessionOutputGap, SessionOutputParams,
    SessionOutputResult, SessionReleaseAgentParams, SessionReleaseAgentResult,
    SessionReportAgentParams, SessionReportAgentResult, SessionReportNativeIdParams,
    SessionReportNativeIdResult, SessionResizeParams, SessionResizeResult, SessionRuntimeIdentity,
    SessionScreenParams, SessionScreenResult, SessionSetMetadataParams, SessionSetMetadataResult,
    SessionState, SessionStopResult, SessionWaitParams, SessionWaitReason, SessionWaitResult,
    SessionWarning, SessionWarningKind, StateSource, TerminalCursor, TerminalDimensions,
    TerminalWatermark, MAX_CONTROL_LINE_BYTES, MAX_REQUEST_ID_BYTES, MAX_RUNTIME_ID_BYTES,
    MAX_SESSION_ID_BYTES, MAX_SESSION_INPUT_BYTES, MAX_SESSION_OUTPUT_BYTES,
    MAX_SESSION_SCREEN_RESPONSE_BYTES, MAX_SESSION_WAIT_MS,
    OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES, PROTOCOL_VERSION,
    SESSION_OUTPUT_METADATA_HEADROOM_BYTES, SUPPORTED_PROTOCOL_VERSIONS,
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

fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[expect(
    clippy::too_many_arguments,
    reason = "the helper varies independent output invariants in focused tests"
)]
fn output_result(
    session_id: String,
    runtime_id: String,
    data_base64: String,
    history_start: u64,
    start: u64,
    next: u64,
    runtime_end: u64,
    gap: Option<SessionOutputGap>,
    has_more: bool,
) -> Result<SessionOutputResult, ObservationParamsError> {
    SessionOutputResult::new(
        SessionId(session_id),
        SessionRuntimeIdentity::new(runtime_id, RuntimeGeneration::new(1))?,
        OutputOffset::new(history_start),
        OutputOffset::new(start),
        OutputOffset::new(next),
        OutputOffset::new(runtime_end),
        data_base64,
        gap,
        has_more,
        false,
    )
}

fn running_shell_session(exit_code: Option<i32>) -> SessionInfo {
    SessionInfo {
        name: None,
        id: SessionId("s-42".to_owned()),
        external: Some(false),
        capabilities: SessionCapabilities::default(),
        agent: "shell".to_owned(),
        agent_base: AgentKind::Shell,
        cwd: PathBuf::from("/workspace/project"),
        cwd_source: Some(CwdSource::Launch),
        pid: 4242,
        runtime: None,
        cols: 120,
        rows: 40,
        state: SessionState::Running,
        state_source: StateSource::Process,
        activity: None,
        active_agent: None,
        active_agent_base: None,
        active_agent_pid: None,
        active_agent_session_id: None,
        active_agent_session_path: None,
        native_session_id: None,
        native_session_path: None,
        project_id: None,
        project_label: None,
        is_linked_worktree: None,
        repo: None,
        branch: None,
        worktree_path: None,
        warnings: Vec::new(),
        metadata: BTreeMap::new(),
        created_at: "2026-06-17T10:00:00Z".to_owned(),
        updated_at: "2026-06-17T10:01:00Z".to_owned(),
        exit_code,
    }
}

fn notification_source() -> NotificationSource {
    NotificationSource {
        provider: "codex".to_owned(),
        provider_event: "PermissionRequest".to_owned(),
        host_local_source_id: "hook-42".to_owned(),
    }
}

fn notification_record() -> NotificationRecord {
    NotificationRecord {
        id: NotificationId("n-42".to_owned()),
        source: notification_source(),
        kind: NotificationKind::ApprovalRequired,
        severity: NotificationSeverity::ActionRequired,
        status: NotificationStatus::Unread,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for a tool approval.".to_owned(),
        metadata: BTreeMap::new(),
        created_at: "2026-07-03T09:00:00Z".to_owned(),
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
    }
}

fn notification_kind_policy() -> NotificationKindPolicy {
    NotificationKindPolicy {
        agent_blocked: true,
        approval_required: true,
        turn_completed: false,
        session_finished: false,
        error: true,
        system: false,
    }
}

fn notification_policy() -> NotificationPolicy {
    NotificationPolicy {
        attention_dedupe_window_secs: 90,
        attention_debounce_secs: 7,
        enabled: notification_kind_policy(),
        providers: BTreeMap::from([
            (
                "codex".to_owned(),
                NotificationKindPolicy {
                    agent_blocked: true,
                    approval_required: true,
                    turn_completed: false,
                    session_finished: false,
                    error: true,
                    system: true,
                },
            ),
            (
                "claude".to_owned(),
                NotificationKindPolicy {
                    agent_blocked: true,
                    approval_required: true,
                    turn_completed: true,
                    session_finished: false,
                    error: true,
                    system: false,
                },
            ),
        ]),
        retention: protocol::NotificationRetentionPolicy::default(),
    }
}

#[test]
fn agent_kind_json_shape_roundtrips() {
    let cases = [
        (AgentKind::Shell, json!("shell")),
        (AgentKind::Codex, json!("codex")),
        (AgentKind::Claude, json!("claude")),
        (AgentKind::Hermes, json!("hermes")),
    ];

    for (agent, expected) in cases {
        let value = serde_json::to_value(&agent).expect("serialize agent");
        assert_eq!(value, expected);

        let back = line_roundtrip(&agent);
        assert_eq!(back, agent);
    }
}

#[test]
fn unknown_agent_kind_roundtrips_neutrally_and_is_rejected_for_mutation() {
    let agent: AgentKind = serde_json::from_value(json!("future-agent")).expect("deserialize");
    assert_eq!(agent, AgentKind::Unknown("future-agent".to_owned()));
    assert_eq!(
        serde_json::to_value(&agent).expect("serialize"),
        json!("future-agent")
    );
    for validation in [agent.validate_mutation(), agent.validate_persistence()] {
        let error = validation.expect_err("unknown agent must be presentation-only");
        assert_eq!(error.code, "agent_kind_unsupported");
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
fn public_enum_string_helpers_match_wire_shapes() {
    macro_rules! assert_wire_label {
        ($value:expr, $label:literal) => {{
            let value = $value;
            assert_eq!(value.as_str(), $label);
            assert_eq!(
                serde_json::to_value(&value).expect("serialize enum"),
                json!($label)
            );
        }};
    }

    assert_wire_label!(ProjectSource::Auto, "auto");
    assert_wire_label!(ProjectSource::Manual, "manual");
    assert_wire_label!(SessionState::Starting, "starting");
    assert_wire_label!(SessionState::Running, "running");
    assert_wire_label!(SessionState::Stopped, "stopped");
    assert_wire_label!(SessionState::Done, "done");
    assert_wire_label!(SessionState::Failed, "failed");
    assert_wire_label!(AgentActivity::Working, "working");
    assert_wire_label!(AgentActivity::Blocked, "blocked");
    assert_wire_label!(AgentActivity::Idle, "idle");
    assert_wire_label!(DoctorStatus::Ok, "ok");
    assert_wire_label!(DoctorStatus::Warn, "warn");
    assert_wire_label!(DoctorStatus::Fail, "fail");
    assert_wire_label!(ProviderKind::LinearIssue, "linear_issue");
    assert_wire_label!(ProviderKind::GithubPr, "github_pr");
    assert_wire_label!(ProviderKind::None, "none");
}

#[test]
fn notification_kind_json_shape_roundtrips() {
    let cases = [
        (NotificationKind::AgentBlocked, json!("agent_blocked")),
        (
            NotificationKind::ApprovalRequired,
            json!("approval_required"),
        ),
        (NotificationKind::TurnCompleted, json!("turn_completed")),
        (NotificationKind::SessionFinished, json!("session_finished")),
        (NotificationKind::Error, json!("error")),
        (NotificationKind::System, json!("system")),
    ];

    for (kind, expected) in cases {
        let value = serde_json::to_value(kind).expect("serialize notification kind");
        assert_eq!(value, expected);

        let back = line_roundtrip(&kind);
        assert_eq!(back, kind);
        assert_eq!(kind.as_str(), expected.as_str().expect("string label"));
    }
}

#[test]
fn notification_severity_json_shape_roundtrips() {
    let cases = [
        (NotificationSeverity::Info, json!("info")),
        (NotificationSeverity::Success, json!("success")),
        (NotificationSeverity::Warning, json!("warning")),
        (NotificationSeverity::Error, json!("error")),
        (
            NotificationSeverity::ActionRequired,
            json!("action_required"),
        ),
    ];

    for (severity, expected) in cases {
        let value = serde_json::to_value(severity).expect("serialize notification severity");
        assert_eq!(value, expected);

        let back = line_roundtrip(&severity);
        assert_eq!(back, severity);
        assert_eq!(severity.as_str(), expected.as_str().expect("string label"));
    }
}

#[test]
fn notification_status_json_shape_roundtrips() {
    let cases = [
        (NotificationStatus::Unread, json!("unread")),
        (NotificationStatus::Read, json!("read")),
        (NotificationStatus::Acknowledged, json!("acknowledged")),
        (NotificationStatus::Archived, json!("archived")),
        (NotificationStatus::Deleted, json!("deleted")),
    ];

    for (status, expected) in cases {
        let value = serde_json::to_value(status).expect("serialize notification status");
        assert_eq!(value, expected);

        let back = line_roundtrip(&status);
        assert_eq!(back, status);
        assert_eq!(status.as_str(), expected.as_str().expect("string label"));
    }
}

#[test]
fn notification_record_json_shape_roundtrips() {
    let record = notification_record();

    let value = serde_json::to_value(&record).expect("serialize notification record");
    assert_eq!(
        value,
        json!({
            "id": "n-42",
            "source": {
                "provider": "codex",
                "provider_event": "PermissionRequest",
                "host_local_source_id": "hook-42"
            },
            "kind": "approval_required",
            "severity": "action_required",
            "status": "unread",
            "title": "Approval required",
            "body": "Codex is waiting for a tool approval.",
            "created_at": "2026-07-03T09:00:00Z"
        })
    );

    let back = line_roundtrip(&record);
    assert_eq!(back, record);
}

#[test]
fn notification_record_omits_absent_optional_fields() {
    let record = notification_record();

    let value = serde_json::to_value(&record).expect("serialize notification record");
    for field in [
        "session_id",
        "agent_kind",
        "source_id",
        "dedupe_key",
        "project_id",
        "read_at",
        "acked_at",
        "archived_at",
        "deleted_at",
        "superseded_by",
        "metadata",
    ] {
        assert!(
            !value
                .as_object()
                .expect("notification record object")
                .contains_key(field),
            "absent optional notification field must not appear on the wire: {field}"
        );
    }
}

#[test]
fn notification_record_metadata_roundtrips_when_present() {
    let mut record = notification_record();
    record.metadata = metadata(&[("provider", "codex"), ("reason", "approval")]);

    let value = serde_json::to_value(&record).expect("serialize notification record");
    assert_eq!(
        value["metadata"],
        json!({
            "provider": "codex",
            "reason": "approval"
        })
    );

    let back = line_roundtrip(&record);
    assert_eq!(back, record);
}

#[test]
fn notification_create_params_json_shape_roundtrips() {
    let params = NotificationCreateParams {
        source: notification_source(),
        kind: NotificationKind::ApprovalRequired,
        severity: NotificationSeverity::ActionRequired,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for a tool approval.".to_owned(),
        session_id: Some(SessionId("s-42".to_owned())),
        agent_kind: Some(AgentKind::Codex),
        source_id: Some("provider-event-7".to_owned()),
        dedupe_key: None,
        project_id: Some("p-42".to_owned()),
        metadata: BTreeMap::new(),
    };

    let value = serde_json::to_value(&params).expect("serialize notification create params");
    assert_eq!(
        value,
        json!({
            "source": {
                "provider": "codex",
                "provider_event": "PermissionRequest",
                "host_local_source_id": "hook-42"
            },
            "kind": "approval_required",
            "severity": "action_required",
            "title": "Approval required",
            "body": "Codex is waiting for a tool approval.",
            "session_id": "s-42",
            "agent_kind": "codex",
            "source_id": "provider-event-7",
            "project_id": "p-42"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_create_params_metadata_roundtrips_when_present() {
    let params = NotificationCreateParams {
        source: notification_source(),
        kind: NotificationKind::ApprovalRequired,
        severity: NotificationSeverity::ActionRequired,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for a tool approval.".to_owned(),
        session_id: Some(SessionId("s-42".to_owned())),
        agent_kind: Some(AgentKind::Codex),
        source_id: Some("provider-event-7".to_owned()),
        dedupe_key: None,
        project_id: Some("p-42".to_owned()),
        metadata: metadata(&[("provider", "codex"), ("reason", "approval")]),
    };

    let value = serde_json::to_value(&params).expect("serialize notification create params");
    assert_eq!(
        value["metadata"],
        json!({
            "provider": "codex",
            "reason": "approval"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_create_params_carries_source_independent_dedupe_key() {
    let params = NotificationCreateParams {
        source: notification_source(),
        kind: NotificationKind::AgentBlocked,
        severity: NotificationSeverity::Warning,
        title: "Agent blocked".to_owned(),
        body: "The agent is waiting for input.".to_owned(),
        session_id: Some(SessionId("s-42".to_owned())),
        agent_kind: Some(AgentKind::Codex),
        source_id: Some("provider-event-7".to_owned()),
        dedupe_key: Some("attention:s-42:approval".to_owned()),
        project_id: Some("p-42".to_owned()),
        metadata: BTreeMap::new(),
    };

    let value = serde_json::to_value(&params).expect("serialize notification create params");
    assert_eq!(value["dedupe_key"], json!("attention:s-42:approval"));

    let back = line_roundtrip(&params);
    assert_eq!(back.dedupe_key.as_deref(), Some("attention:s-42:approval"));
}

#[test]
fn notification_create_result_can_return_existing_record() {
    let result = NotificationCreateResult {
        created: false,
        record: notification_record(),
    };

    let value = serde_json::to_value(&result).expect("serialize notification create result");
    assert_eq!(value["created"], json!(false));
    assert_eq!(value["record"]["id"], json!("n-42"));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn notification_list_params_json_shape_roundtrips() {
    let params = NotificationListParams {
        status: Some(NotificationStatus::Unread),
        kind: Some(NotificationKind::ApprovalRequired),
        severity: Some(NotificationSeverity::ActionRequired),
        provider: Some("codex".to_owned()),
        session_id: Some(SessionId("s-42".to_owned())),
        created_after: Some("2026-07-03T08:00:00Z".to_owned()),
        created_before: Some("2026-07-03T10:00:00Z".to_owned()),
        limit: Some(50),
        cursor: Some("page-2".to_owned()),
    };

    let value = serde_json::to_value(&params).expect("serialize notification list params");
    assert_eq!(
        value,
        json!({
            "status": "unread",
            "kind": "approval_required",
            "severity": "action_required",
            "provider": "codex",
            "session_id": "s-42",
            "created_after": "2026-07-03T08:00:00Z",
            "created_before": "2026-07-03T10:00:00Z",
            "limit": 50,
            "cursor": "page-2"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_update_params_json_shape_roundtrips() {
    let params = NotificationUpdateParams {
        id: NotificationId("n-42".to_owned()),
        status: NotificationStatus::Acknowledged,
    };

    let value = serde_json::to_value(&params).expect("serialize notification update params");
    assert_eq!(
        value,
        json!({
            "id": "n-42",
            "status": "acknowledged"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_delete_params_json_shape_roundtrips() {
    let params = NotificationDeleteParams {
        id: NotificationId("n-42".to_owned()),
    };

    let value = serde_json::to_value(&params).expect("serialize notification delete params");
    assert_eq!(
        value,
        json!({
            "id": "n-42"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_policy_params_json_shape_roundtrips() {
    let params = NotificationPolicyParams {
        policy: notification_policy(),
    };

    let value = serde_json::to_value(&params).expect("serialize notification policy params");
    assert_eq!(
        value,
        json!({
            "policy": {
                "attention_dedupe_window_secs": 90,
                "attention_debounce_secs": 7,
                "enabled": {
                    "agent_blocked": true,
                    "approval_required": true,
                    "turn_completed": false,
                    "session_finished": false,
                    "error": true,
                    "system": false
                },
                "providers": {
                    "codex": {
                        "agent_blocked": true,
                        "approval_required": true,
                        "turn_completed": false,
                        "session_finished": false,
                        "error": true,
                        "system": true
                    },
                    "claude": {
                        "agent_blocked": true,
                        "approval_required": true,
                        "turn_completed": true,
                        "session_finished": false,
                        "error": true,
                        "system": false
                    }
                },
                "retention": {
                    "sweep_interval_secs": 21600,
                    "info_ttl_secs": 259_200,
                    "warning_ttl_secs": 1_209_600,
                    "resolved_attention_ttl_secs": 604_800,
                    "resolved_error_ttl_secs": 2_592_000,
                    "archived_ttl_secs": 7_776_000,
                    "compaction_min_actions": 1000
                }
            }
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_policy_carries_attention_dedupe_window_secs() {
    let policy = notification_policy();

    let value = serde_json::to_value(&policy).expect("serialize notification policy");
    assert_eq!(value["attention_dedupe_window_secs"], json!(90));

    let back = line_roundtrip(&policy);
    assert_eq!(back.attention_dedupe_window_secs, 90);
}

#[test]
fn notification_policy_carries_attention_debounce_secs() {
    let policy = notification_policy();

    let value = serde_json::to_value(&policy).expect("serialize notification policy");
    assert_eq!(value["attention_debounce_secs"], json!(7));

    let back = line_roundtrip(&policy);
    assert_eq!(back.attention_debounce_secs, 7);
}

#[test]
fn notification_policy_defaults_attention_debounce_secs_when_field_absent() {
    // A policy JSON persisted before the debounce field existed must still load,
    // backfilling the default rather than failing to deserialize.
    let value = json!({
        "attention_dedupe_window_secs": 120,
        "enabled": {
            "agent_blocked": true,
            "approval_required": true,
            "turn_completed": false,
            "session_finished": false,
            "error": true,
            "system": false
        }
    });

    let policy: NotificationPolicy =
        serde_json::from_value(value).expect("legacy policy without debounce field loads");

    // 5 is the documented default backfilled by `default_attention_debounce_secs`.
    assert_eq!(policy.attention_debounce_secs, 5);
    assert_eq!(policy.attention_dedupe_window_secs, 120);
    assert_eq!(policy.retention, NotificationRetentionPolicy::default());
}

#[test]
fn notification_retention_policy_roundtrips_and_rejects_unknown_fields() {
    let retention = NotificationRetentionPolicy::default();

    assert_eq!(line_roundtrip(&retention), retention);
    let mut value = serde_json::to_value(retention).expect("serialize retention policy");
    value["future_ttl_secs"] = json!(60);
    serde_json::from_value::<NotificationRetentionPolicy>(value)
        .expect_err("unknown retention policy field must fail");
}

#[test]
fn notification_retention_params_json_shape_roundtrips() {
    let params = NotificationRetentionParams {
        dry_run: true,
        status: Some(NotificationStatus::Archived),
        before: Some("2026-08-01T00:00:00Z".to_owned()),
        limit: Some(100),
    };

    let value = serde_json::to_value(&params).expect("serialize notification retention params");
    assert_eq!(
        value,
        json!({
            "dry_run": true,
            "status": "archived",
            "before": "2026-08-01T00:00:00Z",
            "limit": 100
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn notification_result_types_json_shape_roundtrip() {
    let list = NotificationListResult {
        notifications: vec![notification_record()],
        next_cursor: Some("page-2".to_owned()),
    };
    let update = NotificationUpdateResult {
        record: notification_record(),
    };
    let delete = NotificationDeleteResult {
        id: NotificationId("n-42".to_owned()),
        deleted: true,
    };
    let policy = NotificationPolicyResult {
        policy: notification_policy(),
    };
    let retention = NotificationRetentionResult {
        dry_run: true,
        pruned: vec![NotificationId("n-42".to_owned())],
    };

    assert_eq!(line_roundtrip(&list), list);
    assert_eq!(line_roundtrip(&update), update);
    assert_eq!(line_roundtrip(&delete), delete);
    assert_eq!(line_roundtrip(&policy), policy);
    assert_eq!(line_roundtrip(&retention), retention);
}

#[test]
fn notification_method_names_are_stable() {
    assert_eq!(method::NOTIFICATION_CREATE, "notification.create");
    assert_eq!(method::NOTIFICATION_LIST, "notification.list");
    assert_eq!(method::NOTIFICATION_UPDATE, "notification.update");
    assert_eq!(method::NOTIFICATION_DELETE, "notification.delete");
    assert_eq!(method::NOTIFICATION_POLICY_GET, "notification.policy.get");
    assert_eq!(method::NOTIFICATION_POLICY_SET, "notification.policy.set");
    assert_eq!(
        method::NOTIFICATION_RETENTION_PRUNE,
        "notification.retention.prune"
    );
}

#[test]
fn notification_created_event_carries_record_in_flattened_payload() {
    let event = Event::new(
        PROTOCOL_VERSION,
        event::NOTIFICATION_CREATED,
        serde_json::to_value(NotificationCreatedEvent {
            record: notification_record(),
        })
        .expect("serialize notification created event"),
    )
    .expect("valid event");

    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.event(), event::NOTIFICATION_CREATED);

    let value = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(value["event"], json!("notification_created"));
    assert_eq!(value["record"]["id"], json!("n-42"));
    assert!(
        !value
            .as_object()
            .expect("event object")
            .contains_key("payload"),
        "event payload fields must be flattened: {value}"
    );
}

#[test]
fn notification_updated_event_carries_record_in_flattened_payload() {
    let mut record = notification_record();
    record.status = NotificationStatus::Acknowledged;
    record.acked_at = Some("2026-07-03T09:05:00Z".to_owned());

    let event = Event::new(
        PROTOCOL_VERSION,
        event::NOTIFICATION_UPDATED,
        serde_json::to_value(NotificationUpdatedEvent { record })
            .expect("serialize notification updated event"),
    )
    .expect("valid event");

    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.event(), event::NOTIFICATION_UPDATED);

    let value = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(value["event"], json!("notification_updated"));
    assert_eq!(value["record"]["status"], json!("acknowledged"));
    assert!(
        !value
            .as_object()
            .expect("event object")
            .contains_key("payload"),
        "event payload fields must be flattened: {value}"
    );
}

#[test]
fn notification_deleted_event_carries_id_in_flattened_payload() {
    let event = Event::new(
        PROTOCOL_VERSION,
        event::NOTIFICATION_DELETED,
        serde_json::to_value(NotificationDeletedEvent {
            notification_id: NotificationId("n-42".to_owned()),
        })
        .expect("serialize notification deleted event"),
    )
    .expect("valid event");

    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.event(), event::NOTIFICATION_DELETED);

    let value = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(
        value,
        json!({
            "v": PROTOCOL_VERSION,
            "event": "notification_deleted",
            "notification_id": "n-42"
        })
    );
    assert!(
        !value
            .as_object()
            .expect("event object")
            .contains_key("payload"),
        "event payload fields must be flattened: {value}"
    );
}

#[test]
fn session_new_params_json_shape_roundtrips() {
    let params = SessionNewParams {
        agent: "shell".to_owned(),
        name: None,
        cwd: Some(PathBuf::from("/workspace/project")),
        cols: 120,
        rows: 40,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        metadata: BTreeMap::new(),
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
fn session_new_params_roundtrips_with_metadata() {
    let params = SessionNewParams {
        agent: "shell".to_owned(),
        name: None,
        cwd: Some(PathBuf::from("/workspace/project")),
        cols: 120,
        rows: 40,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        metadata: metadata(&[("owner", "cli"), ("ticket", "DMD-1356")]),
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(
        value,
        json!({
            "agent": "shell",
            "cwd": "/workspace/project",
            "cols": 120,
            "rows": 40,
            "metadata": {
                "owner": "cli",
                "ticket": "DMD-1356"
            }
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_new_params_roundtrips_with_worktree_fields() {
    let params = SessionNewParams {
        agent: "claude".to_owned(),
        name: None,
        cwd: None,
        cols: 80,
        rows: 24,
        project: None,
        repo: Some(PathBuf::from("/workspace/project")),
        branch: Some("feature/login".to_owned()),
        base_branch: Some("main".to_owned()),
        input: None,
        metadata: BTreeMap::new(),
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
        agent: "shell".to_owned(),
        name: None,
        cwd: None,
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        metadata: BTreeMap::new(),
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    let object = value.as_object().expect("params object");
    for absent in ["cwd", "repo", "branch", "base_branch", "input"] {
        assert!(
            !object.contains_key(absent),
            "absent {absent} must be omitted: {value}"
        );
    }

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_new_params_roundtrips_with_initial_input() {
    let params = SessionNewParams {
        agent: "shell".to_owned(),
        name: None,
        cwd: Some(PathBuf::from("/workspace/project")),
        cols: 120,
        rows: 40,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: Some("run tests".to_owned()),
        metadata: BTreeMap::new(),
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(
        value,
        json!({
            "agent": "shell",
            "cwd": "/workspace/project",
            "cols": 120,
            "rows": 40,
            "input": "run tests"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_fork_params_json_shape_roundtrips() {
    let params = SessionForkParams {
        session_id: SessionId("s-42".to_owned()),
        name: Some("forked review".to_owned()),
        cwd_mode: ForkCwdMode::Same,
        cols: 100,
        rows: 30,
    };

    let value = serde_json::to_value(&params).expect("serialize fork params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "name": "forked review",
            "cwd_mode": "same",
            "cols": 100,
            "rows": 30
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_fork_params_default_to_same_cwd_mode() {
    let params: SessionForkParams = serde_json::from_value(json!({
        "session_id": "s-42",
        "cols": 100,
        "rows": 30
    }))
    .expect("deserialize fork params without cwd_mode");

    assert_eq!(params.cwd_mode, ForkCwdMode::Same);
}

#[test]
fn session_fork_result_flattens_session_info() {
    let result = SessionForkResult {
        session: SessionInfo {
            id: SessionId("s-99".to_owned()),
            ..running_shell_session(None)
        },
        applied_input: None,
    };

    let value = serde_json::to_value(&result).expect("serialize fork result");
    assert_eq!(value["id"], "s-99");
    assert!(
        value.get("session").is_none(),
        "fork result must flatten SessionInfo, not wrap it: {value}"
    );
    assert!(
        value.get("applied_input").is_none(),
        "absent applied_input must stay omitted: {value}"
    );

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn session_list_params_roundtrips_with_filters() {
    let params = SessionListParams {
        filters: vec![
            SessionListFilter::State(SessionState::Running),
            SessionListFilter::Agent("codex".to_owned()),
            SessionListFilter::Activity(AgentActivity::Blocked),
            SessionListFilter::Id("s-42".to_owned()),
        ],
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(
        value,
        json!({
            "filters": [
                { "key": "state", "value": "running" },
                { "key": "agent", "value": "codex" },
                { "key": "activity", "value": "blocked" },
                { "key": "id", "value": "s-42" }
            ]
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_list_agent_filter_roundtrips_and_matches_launch_or_active_agent_identity() {
    let session = SessionInfo {
        agent: "shell-main".to_owned(),
        agent_base: AgentKind::Shell,
        active_agent: Some("codex-gpt-5".to_owned()),
        active_agent_base: Some(AgentKind::Codex),
        ..running_shell_session(None)
    };

    for agent in ["shell-main", "shell", "codex-gpt-5", "codex"] {
        let filter = line_roundtrip(&SessionListFilter::Agent(agent.to_owned()));
        assert!(
            filter.matches(&session),
            "agent filter {agent:?} must match launch or active identity"
        );
    }

    let filter = line_roundtrip(&SessionListFilter::Agent("claude".to_owned()));
    assert!(
        !filter.matches(&session),
        "agent filter must reject unrelated identities"
    );
}

#[test]
fn session_list_params_omits_empty_filters() {
    let params = SessionListParams::default();

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(value, json!({}));

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_list_filter_unknown_key_is_a_deserialization_error() {
    // An unknown filter key must fail to deserialize (the adjacently-tagged enum
    // rejects it), so the daemon answers with a typed usage error rather than
    // silently dropping the filter and returning every session.
    let value = json!({ "filters": [{ "key": "cwd", "value": "/workspace" }] });
    assert!(
        serde_json::from_value::<SessionListParams>(value).is_err(),
        "an unknown filter key must be a deserialization error, not a dropped filter"
    );
}

#[test]
fn session_list_filter_bad_value_is_a_deserialization_error() {
    // A known key with a value outside the closed enum (here an invalid state)
    // must also fail to deserialize, not match nothing silently.
    let value = json!({ "filters": [{ "key": "state", "value": "paused" }] });
    assert!(
        serde_json::from_value::<SessionListParams>(value).is_err(),
        "an out-of-range filter value must be a deserialization error"
    );
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
            "external": false,
            "agent": "shell",
            "agent_base": "shell",
            "capabilities": {"resume": false, "fork": false},
            "cwd": "/workspace/project",
            "cwd_source": "launch",
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
fn session_info_external_marker_roundtrips() {
    let managed = SessionInfo {
        external: Some(false),
        ..running_shell_session(None)
    };
    let external = SessionInfo {
        id: SessionId("ext-4242".to_owned()),
        external: Some(true),
        ..running_shell_session(None)
    };

    assert_eq!(
        serde_json::to_value(&managed).expect("serialize managed session")["external"],
        json!(false)
    );
    assert_eq!(
        serde_json::to_value(&external).expect("serialize external session")["external"],
        json!(true)
    );
    assert_eq!(line_roundtrip(&managed), managed);
    assert_eq!(line_roundtrip(&external), external);
}

#[test]
fn session_info_json_shape_roundtrips_with_metadata() {
    let info = SessionInfo {
        metadata: metadata(&[("owner", "cli"), ("ticket", "DMD-1356")]),
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(value["metadata"]["owner"], json!("cli"));
    assert_eq!(value["metadata"]["ticket"], json!("DMD-1356"));

    let back = line_roundtrip(&info);
    assert_eq!(back, info);
}

#[test]
fn session_info_json_shape_roundtrips_with_exit_code() {
    let info = SessionInfo {
        state: SessionState::Done,
        name: None,
        state_source: StateSource::OscTitle,
        exit_code: Some(0),
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(
        value,
        json!({
            "id": "s-42",
            "external": false,
            "agent": "shell",
            "agent_base": "shell",
            "capabilities": {"resume": false, "fork": false},
            "cwd": "/workspace/project",
            "cwd_source": "launch",
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
    assert_eq!(MAX_SESSION_INPUT_BYTES, MAX_CONTROL_LINE_BYTES / 4);
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
    assert_eq!(method::SESSION_REPORT_NATIVE_ID, "session.report_native_id");
}

#[test]
fn session_report_agent_method_names_are_stable() {
    assert_eq!(method::SESSION_REPORT_AGENT, "session.report_agent");
    assert_eq!(method::SESSION_RELEASE_AGENT, "session.release_agent");
}

#[test]
fn session_set_metadata_method_name_is_stable() {
    assert_eq!(method::SESSION_SET_METADATA, "session.set_metadata");
}

#[test]
fn session_report_agent_params_roundtrip_with_optional_native_refs() {
    let params = SessionReportAgentParams {
        session_id: SessionId("s-42".to_owned()),
        source: "pohunek:codex".to_owned(),
        agent: "codex".to_owned(),
        activity: Some(AgentActivity::Working),
        seq: Some(ReportSequence::new(123)),
        pid: Some(9001),
        agent_session_id: Some("codex-native".to_owned()),
        agent_session_path: None,
    };

    let value = serde_json::to_value(&params).expect("serialize report params");
    assert_eq!(value["session_id"], json!("s-42"));
    assert_eq!(value["source"], json!("pohunek:codex"));
    assert_eq!(value["agent"], json!("codex"));
    assert_eq!(value["activity"], json!("working"));
    assert_eq!(value["seq"], json!("123"));
    assert_eq!(value["pid"], json!(9001));
    assert_eq!(value["agent_session_id"], json!("codex-native"));
    assert!(
        !value
            .as_object()
            .expect("params object")
            .contains_key("agent_session_path"),
        "absent agent_session_path must be omitted: {value}"
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_release_agent_params_omits_absent_sequence() {
    let params = SessionReleaseAgentParams {
        session_id: SessionId("s-42".to_owned()),
        source: "pohunek:codex".to_owned(),
        agent: "codex".to_owned(),
        seq: None,
    };

    let value = serde_json::to_value(&params).expect("serialize release params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "source": "pohunek:codex",
            "agent": "codex"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_report_and_release_agent_results_roundtrip() {
    for result in [
        SessionReportAgentResult { recorded: true },
        SessionReportAgentResult { recorded: false },
    ] {
        let value = serde_json::to_value(&result).expect("serialize report result");
        assert_eq!(value, json!({ "recorded": result.recorded }));
        assert_eq!(line_roundtrip(&result), result);
    }

    for result in [
        SessionReleaseAgentResult { released: true },
        SessionReleaseAgentResult { released: false },
    ] {
        let value = serde_json::to_value(&result).expect("serialize release result");
        assert_eq!(value, json!({ "released": result.released }));
        assert_eq!(line_roundtrip(&result), result);
    }
}

#[test]
fn session_report_native_id_params_roundtrips_with_transcript_path() {
    let params = SessionReportNativeIdParams::new(
        SessionId("s-42".to_owned()),
        "runtime-42",
        "claude",
        4242,
        ProcessStartIdentity::new(777),
        ReportSequence::new(3),
        "2026-07-27T12:00:10Z",
        "claude-native-abc",
        Some("/home/user/.claude/transcripts/abc.jsonl".to_owned()),
    )
    .expect("valid native identity report");

    let value = serde_json::to_value(&params).expect("serialize report params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "runtime_id": "runtime-42",
            "agent": "claude",
            "pid": 4242,
            "pid_start_identity": "777",
            "sequence": "3",
            "expires_at": "2026-07-27T12:00:10Z",
            "native_session_id": "claude-native-abc",
            "transcript_path": "/home/user/.claude/transcripts/abc.jsonl"
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_report_native_id_params_omits_absent_transcript_path() {
    let params = SessionReportNativeIdParams::new(
        SessionId("s-7".to_owned()),
        "runtime-7",
        "codex",
        7007,
        ProcessStartIdentity::new(700),
        ReportSequence::new(1),
        "2026-07-27T12:00:10Z",
        "codex-native-xyz",
        None,
    )
    .expect("valid native identity report");

    let value = serde_json::to_value(&params).expect("serialize report params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-7",
            "runtime_id": "runtime-7",
            "agent": "codex",
            "pid": 7007,
            "pid_start_identity": "700",
            "sequence": "1",
            "expires_at": "2026-07-27T12:00:10Z",
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
fn session_info_roundtrips_with_active_agent_fields() {
    let info = SessionInfo {
        active_agent: Some("codex".to_owned()),
        active_agent_base: Some(AgentKind::Codex),
        active_agent_pid: Some(9001),
        active_agent_session_id: Some("codex-native".to_owned()),
        active_agent_session_path: None,
        ..running_shell_session(None)
    };

    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(value["agent"], json!("shell"));
    assert_eq!(value["active_agent"], json!("codex"));
    assert_eq!(value["active_agent_base"], json!("codex"));
    assert_eq!(value["active_agent_pid"], json!(9001));
    assert_eq!(value["active_agent_session_id"], json!("codex-native"));
    assert!(
        !value
            .as_object()
            .expect("session info object")
            .contains_key("active_agent_session_path"),
        "absent active_agent_session_path must be omitted: {value}"
    );

    let back = line_roundtrip(&info);
    assert_eq!(back, info);
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
        (SessionWarningKind::Hook, json!("hook")),
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
        name: None,
        repo: Some(PathBuf::from("/workspace/project")),
        branch: Some("feature/login".to_owned()),
        worktree_path: Some(PathBuf::from("/data/worktrees/s-42-project-feature-login")),
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
                hook_path: "/home/user/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                config_paths: vec!["/home/user/.claude/settings.json".to_owned()],
            },
            IntegrationInstallReport {
                agent: AgentKind::Codex,
                hook_path: "/home/user/.codex/pohunek-agent-state.sh".to_owned(),
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
        initial_dimensions: Some(TerminalDimensions::new(120, 40).expect("valid dimensions")),
        origin_session_id: None,
        origin_daemon_id: None,
        origin_worker_id: None,
    };

    // Optional origin fields remain absent while a known physical geometry is
    // carried in the attach handshake.
    let value = serde_json::to_value(&params).expect("serialize attach params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "initial_dimensions": { "cols": 120, "rows": 40 }
        })
    );

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
    let result = SessionDetachResult {
        detached: true,
        error: None,
    };

    let value = serde_json::to_value(&result).expect("serialize detach result");
    assert_eq!(value, json!({ "detached": true }));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);

    let failed = SessionDetachResult {
        detached: false,
        error: Some(ProtocolError::new(
            ErrorClass::Runtime,
            "worker_attach_stream_failed",
            "worker attach stream failed",
            None,
        )),
    };
    let value = serde_json::to_value(&failed).expect("serialize failed detach result");
    assert_eq!(
        value,
        json!({
            "detached": false,
            "error": {
                "class": "runtime",
                "code": "worker_attach_stream_failed",
                "msg": "worker attach stream failed"
            }
        })
    );
    assert_eq!(line_roundtrip(&failed), failed);
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
                "external": false,
                "agent": "shell",
                "agent_base": "shell",
                "capabilities": {"resume": false, "fork": false},
                "cwd": "/workspace/project",
                "cwd_source": "launch",
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
fn session_set_metadata_params_deserializes_null_to_none_and_roundtrips() {
    let value = json!({
        "session_id": "s-42",
        "metadata": {
            "owner": "cli",
            "ticket": null
        }
    });

    let params: SessionSetMetadataParams =
        serde_json::from_value(value.clone()).expect("deserialize set metadata params");
    assert_eq!(
        params
            .metadata
            .get("owner")
            .and_then(|value| value.as_deref()),
        Some("cli")
    );
    assert_eq!(params.metadata.get("ticket"), Some(&None));

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
    assert_eq!(
        serde_json::to_value(&params).expect("serialize set metadata params"),
        value
    );
}

#[test]
fn session_set_metadata_params_requires_metadata_field() {
    let value = json!({ "session_id": "s-42" });

    assert!(
        serde_json::from_value::<SessionSetMetadataParams>(value).is_err(),
        "session.set_metadata params must require the metadata patch field"
    );
}

#[test]
fn session_set_metadata_params_serializes_empty_metadata_patch() {
    let params = SessionSetMetadataParams {
        session_id: SessionId("s-42".to_owned()),
        metadata: BTreeMap::new(),
    };

    let value = serde_json::to_value(&params).expect("serialize set metadata params");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "metadata": {}
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn session_set_metadata_result_wraps_updated_session_info() {
    let session = SessionInfo {
        metadata: metadata(&[("owner", "cli"), ("ticket", "DMD-1356")]),
        ..running_shell_session(None)
    };
    let result = SessionSetMetadataResult {
        session: session.clone(),
    };

    let value = serde_json::to_value(&result).expect("serialize set metadata result");
    assert_eq!(value["session"]["metadata"]["owner"], json!("cli"));
    assert_eq!(value["session"]["metadata"]["ticket"], json!("DMD-1356"));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
    assert_eq!(back.session, session);
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
    let event = Event::new(
        PROTOCOL_VERSION,
        event::SESSION_CREATED,
        json!({ "session": session }),
    )
    .expect("valid event");

    let back = line_roundtrip(&event);
    assert_eq!(back, event);
    assert_eq!(back.event(), event::SESSION_CREATED);

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
    )
    .expect("valid request");
    let back = line_roundtrip(&req);
    assert_eq!(req, back);
    assert_eq!(back.version_range(), SUPPORTED_PROTOCOL_VERSIONS);
    assert_eq!(back.id(), "req-7f3");
    assert_eq!(back.method(), "session.new");
    assert_eq!(back.params()["agent"], json!("claude"));
}

#[test]
fn request_missing_params_defaults_to_null() {
    // A parameterless method may omit `params` entirely on the wire.
    let raw = r#"{"v":{"minimum":2,"maximum":2},"id":"req-1","method":"daemon.health"}"#;
    let req: Request = serde_json::from_str(raw).expect("deserialize");
    assert_eq!(req.method(), method::DAEMON_HEALTH);
    assert_eq!(req.params(), &Value::Null);
}

#[test]
fn ok_response_roundtrip() {
    let resp = Response::ok(
        PROTOCOL_VERSION,
        "req-7f3",
        json!({ "session_id": "s-42", "state": "working" }),
    )
    .expect("valid response id");
    let back = line_roundtrip(&resp);
    assert_eq!(resp, back);
    assert_eq!(back.version(), PROTOCOL_VERSION);
    assert_eq!(back.id(), "req-7f3");
    assert_eq!(
        back.result().expect("expected ok variant")["session_id"],
        json!("s-42")
    );
}

#[test]
fn err_response_roundtrip() {
    let err = ProtocolError::new(
        ErrorClass::Runtime,
        "agent_binary_missing",
        "claude not found on PATH",
        Some("install claude".to_owned()),
    );
    let resp = Response::err(PROTOCOL_VERSION, "req-7f3", err.clone()).expect("valid response id");
    let back = line_roundtrip(&resp);
    assert_eq!(resp, back);
    assert_eq!(back.version(), PROTOCOL_VERSION);
    assert_eq!(back.id(), "req-7f3");
    let got = back.result().expect_err("expected err variant");
    assert_eq!(got, &err);
    assert_eq!(got.class, ErrorClass::Runtime);
    assert_eq!(got.code, "agent_binary_missing");
    assert_eq!(got.recover.as_deref(), Some("install claude"));
}

#[test]
fn err_response_without_recover_omits_field() {
    // `recover` is optional and must be omitted from the wire when absent.
    let resp = Response::err(
        PROTOCOL_VERSION,
        "req-2",
        ProtocolError::method_not_found("nope.method"),
    )
    .expect("valid response id");
    let line = serde_json::to_string(&resp).expect("serialize");
    assert!(
        !line.contains("recover"),
        "absent recover hint must not appear on the wire: {line}"
    );
    let back = line_roundtrip(&resp);
    assert_eq!(resp, back);
}

#[test]
fn state_source_report_json_shape_roundtrips() {
    let value = serde_json::to_value(StateSource::Report).expect("serialize state source");
    assert_eq!(value, json!("report"));
    assert_eq!(line_roundtrip(&StateSource::Report), StateSource::Report);
}

#[test]
fn agent_state_event_carries_activity_in_flattened_payload() {
    let event = Event::new(
        PROTOCOL_VERSION,
        event::AGENT_STATE,
        json!({
            "session_id": "s-42",
            "activity": AgentActivity::Blocked,
            "source": StateSource::OscTitle
        }),
    )
    .expect("valid event");
    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.version(), PROTOCOL_VERSION);
    assert_eq!(back.event(), event::AGENT_STATE);

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
        PROTOCOL_VERSION,
        "session_exit",
        json!({ "session_id": "s-7", "exit_code": 0 }),
    )
    .expect("valid event")
    .with_id("req-99")
    .expect("valid event correlation id");
    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.id(), Some("req-99"));
}

#[test]
fn request_unknown_fields_are_rejected() {
    let raw = r#"{"v":{"minimum":2,"maximum":2},"id":"req-1","method":"daemon.health","params":null,"future_field":true}"#;
    serde_json::from_str::<Request>(raw).expect_err("unknown request field must fail");
}

#[test]
fn response_and_event_coordinates_cannot_bypass_validation() {
    for invalid in [
        json!({"v":0,"id":"req-1","ok":{}}),
        json!({"v":2,"id":"","ok":{}}),
        json!({"v":2,"id":"bad id","ok":{}}),
        json!({"v":2,"id":"req-1","ok":{},"err":{"class":"runtime","code":"x","msg":"x"}}),
        json!({"v":2,"id":"req-1","ok":{},"unknown":true}),
    ] {
        serde_json::from_value::<Response>(invalid)
            .expect_err("invalid response coordinates must fail");
    }

    Event::new(PROTOCOL_VERSION, "", json!({})).expect_err("empty event name must fail");
    Event::new(PROTOCOL_VERSION, "agent_state", Value::Null)
        .expect_err("non-object event payload must fail");
    Event::new(PROTOCOL_VERSION, "agent_state", json!({"v": 99}))
        .expect_err("reserved event payload key must fail");
    Event::new(PROTOCOL_VERSION, "agent_state", json!({}))
        .expect("valid event")
        .with_id("bad id")
        .expect_err("invalid event correlation id must fail");

    for invalid in [
        json!({"v":0,"event":"agent_state"}),
        json!({"v":2,"event":""}),
        json!({"v":2,"event":"agent_state","id":"bad id"}),
    ] {
        serde_json::from_value::<Event>(invalid).expect_err("invalid event coordinates must fail");
    }
}

#[test]
fn negotiate_matching_versions_agrees() {
    let agreed = negotiate(SUPPORTED_PROTOCOL_VERSIONS, SUPPORTED_PROTOCOL_VERSIONS)
        .expect("equal ranges agree");
    assert_eq!(agreed, PROTOCOL_VERSION);
}

#[test]
fn negotiate_mismatched_versions_returns_typed_error() {
    let client = ProtocolVersionRange::new(
        ProtocolVersion::new(1).expect("valid version"),
        ProtocolVersion::new(1).expect("valid version"),
    )
    .expect("ordered range");
    let daemon = ProtocolVersionRange::new(
        ProtocolVersion::new(2).expect("valid version"),
        ProtocolVersion::new(2).expect("valid version"),
    )
    .expect("ordered range");
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
    let err = ProtocolError::version_mismatch(
        ProtocolVersionRange::new(
            ProtocolVersion::new(1).expect("valid version"),
            ProtocolVersion::new(1).expect("valid version"),
        )
        .expect("ordered range"),
        ProtocolVersionRange::new(
            ProtocolVersion::new(2).expect("valid version"),
            ProtocolVersion::new(2).expect("valid version"),
        )
        .expect("ordered range"),
    );
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
    assert_eq!(line, "2");
}

#[test]
fn host_inspect_method_name_is_stable() {
    assert_eq!(method::HOST_INSPECT, "host.inspect");
}

#[test]
fn assistant_method_names_are_stable() {
    assert_eq!(method::ASSISTANT_MATERIALIZE, "assistant.materialize");
    assert_eq!(method::DAEMON_DOCTOR, "daemon.doctor");
}

#[test]
fn observation_method_names_and_limits_are_stable() {
    assert_eq!(method::SESSION_SCREEN, "session.screen");
    assert_eq!(method::SESSION_DETECTION, "session.detection");
    assert_eq!(method::SESSION_OUTPUT, "session.output");
    assert_eq!(method::SESSION_WAIT, "session.wait");
    assert_eq!(
        MAX_SESSION_SCREEN_RESPONSE_BYTES + OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES,
        MAX_CONTROL_LINE_BYTES
    );
    const {
        assert!(MAX_SESSION_OUTPUT_BYTES > 0);
    }
    const {
        assert!(MAX_SESSION_OUTPUT_BYTES / 3 * 3 == MAX_SESSION_OUTPUT_BYTES);
    }
    const {
        assert!(
            MAX_SESSION_OUTPUT_BYTES / 3 * 4
                + SESSION_OUTPUT_METADATA_HEADROOM_BYTES
                + OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES
                <= MAX_CONTROL_LINE_BYTES
        );
    }
    const {
        assert!(
            (MAX_SESSION_OUTPUT_BYTES + 3) / 3 * 4
                + SESSION_OUTPUT_METADATA_HEADROOM_BYTES
                + OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES
                > MAX_CONTROL_LINE_BYTES
        );
    }
}

#[test]
fn session_detection_contract_has_exact_wire_shape() {
    let params = SessionDetectionParams::new(SessionId("s-42".to_owned()));
    assert_eq!(
        serde_json::to_value(&params).expect("serialize detection params"),
        json!({"session_id": "s-42"})
    );
    let result = SessionDetectionResult {
        session_id: SessionId("s-42".to_owned()),
        supported_regions: DetectionRegionKind::ALL.to_vec(),
        previews: vec![DetectionRegionPreview {
            kind: DetectionRegionKind::TopNonEmptyLines,
            region: "top_non_empty_lines(8)".to_owned(),
            text: "Do you trust the contents of this directory?".to_owned(),
        }],
    };
    let expected = json!({
        "session_id": "s-42",
        "supported_regions": [
            "osc_title",
            "osc_progress",
            "whole_recent",
            "bottom_lines",
            "bottom_non_empty_lines",
            "top_non_empty_lines",
            "last_non_empty_above_prompt_box",
            "after_last_prompt_marker",
            "prompt_box_body",
            "after_last_horizontal_rule"
        ],
        "previews": [{
            "kind": "top_non_empty_lines",
            "region": "top_non_empty_lines(8)",
            "text": "Do you trust the contents of this directory?"
        }]
    });

    assert_eq!(
        serde_json::to_value(&result).expect("serialize detection result"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionDetectionResult>(expected).expect("parse detection result"),
        result
    );
    serde_json::from_value::<SessionDetectionParams>(json!({
        "session_id": "s-42",
        "unknown": true
    }))
    .expect_err("unknown detection field must fail");
}

#[test]
fn request_id_and_full_screen_envelope_boundaries_are_exact() {
    let exact_id = "r".repeat(MAX_REQUEST_ID_BYTES);
    Request::new(&exact_id, method::DAEMON_HEALTH, Value::Null)
        .expect("maximum request id is valid");
    Request::new("", method::DAEMON_HEALTH, Value::Null).expect_err("empty request id must fail");
    Request::new(
        "r".repeat(MAX_REQUEST_ID_BYTES + 1),
        method::DAEMON_HEALTH,
        Value::Null,
    )
    .expect_err("over-limit request id must fail");
    Request::new("needs\"escaping", method::DAEMON_HEALTH, Value::Null)
        .expect_err("quoted request id must fail");
    Request::new("needs\\escaping", method::DAEMON_HEALTH, Value::Null)
        .expect_err("backslash request id must fail");

    let maximum_version = ProtocolVersion::new(u32::MAX).expect("nonzero version");
    let escaped_count = (MAX_SESSION_SCREEN_RESPONSE_BYTES - 2) / 6;
    let literal_count = (MAX_SESSION_SCREEN_RESPONSE_BYTES - 2) % 6;
    let exact_screen = format!(
        "{}{}",
        "\0".repeat(escaped_count),
        "x".repeat(literal_count)
    );
    assert_eq!(
        serde_json::to_vec(&Value::String(exact_screen.clone()))
            .expect("serialize escaped screen")
            .len(),
        MAX_SESSION_SCREEN_RESPONSE_BYTES
    );
    let exact = Response::ok(
        maximum_version,
        &exact_id,
        Value::String(exact_screen.clone()),
    )
    .expect("valid response");
    assert_eq!(
        serde_json::to_vec(&exact)
            .expect("serialize exact response")
            .len(),
        MAX_CONTROL_LINE_BYTES
    );
    let too_large = Response::ok(
        maximum_version,
        exact_id,
        Value::String(format!("{exact_screen}x")),
    )
    .expect("valid response");
    assert_eq!(
        serde_json::to_vec(&too_large)
            .expect("serialize oversized response")
            .len(),
        MAX_CONTROL_LINE_BYTES + 1
    );
}

#[test]
fn typed_output_result_fits_reserved_envelope_at_exact_raw_limit() {
    let encoded = "A".repeat(MAX_SESSION_OUTPUT_BYTES / 3 * 4);
    let history_start =
        u64::MAX - u64::try_from(MAX_SESSION_OUTPUT_BYTES).expect("output limit fits u64") - 1;
    let start = history_start;
    let next = u64::MAX - 1;
    let runtime_end = u64::MAX;
    let gap = SessionOutputGap::new(
        OutputOffset::new(history_start - 1),
        OutputOffset::new(history_start),
    )
    .expect("valid maximum-width gap");
    let result = SessionOutputResult::new(
        SessionId("\\".repeat(MAX_SESSION_ID_BYTES)),
        SessionRuntimeIdentity::new(
            "\\".repeat(MAX_RUNTIME_ID_BYTES),
            RuntimeGeneration::new(u64::MAX),
        )
        .expect("maximum runtime id is valid"),
        OutputOffset::new(history_start),
        OutputOffset::new(start),
        OutputOffset::new(next),
        OutputOffset::new(runtime_end),
        encoded,
        Some(gap),
        true,
        false,
    )
    .expect("exact protocol output limit is valid");
    let result_value = serde_json::to_value(&result).expect("serialize typed output result");
    let result_len = serde_json::to_vec(&result_value)
        .expect("serialize typed output value")
        .len();
    let encoded_len = result.data_base64().len();
    assert!(
        result_len - encoded_len <= SESSION_OUTPUT_METADATA_HEADROOM_BYTES,
        "maximum typed output metadata must fit the reserved headroom"
    );

    let response = Response::ok(
        ProtocolVersion::new(u32::MAX).expect("nonzero version"),
        "r".repeat(MAX_REQUEST_ID_BYTES),
        result_value,
    )
    .expect("valid response");
    assert!(
        serde_json::to_vec(&response)
            .expect("serialize maximum typed output response")
            .len()
            <= MAX_CONTROL_LINE_BYTES
    );

    let over_limit = SessionOutputResult::new(
        SessionId("\\".repeat(MAX_SESSION_ID_BYTES)),
        SessionRuntimeIdentity::new(
            "\\".repeat(MAX_RUNTIME_ID_BYTES),
            RuntimeGeneration::new(u64::MAX),
        )
        .expect("maximum runtime id is valid"),
        OutputOffset::new(history_start),
        OutputOffset::new(start),
        OutputOffset::new(runtime_end),
        OutputOffset::new(runtime_end),
        format!("{}AA==", "A".repeat(MAX_SESSION_OUTPUT_BYTES / 3 * 4)),
        Some(gap),
        false,
        false,
    )
    .expect_err("exactly one raw byte above the protocol limit must fail");
    assert!(matches!(
        over_limit,
        ObservationParamsError::OutputDataTooLarge { actual, .. }
            if actual == MAX_SESSION_OUTPUT_BYTES + 1
    ));
}

#[test]
fn response_uses_selected_overlap_not_server_maximum() {
    let client = ProtocolVersionRange::new(
        ProtocolVersion::new(2).expect("version"),
        ProtocolVersion::new(3).expect("version"),
    )
    .expect("ordered range");
    let server = ProtocolVersionRange::new(
        ProtocolVersion::new(1).expect("version"),
        ProtocolVersion::new(4).expect("version"),
    )
    .expect("ordered range");
    let selected = negotiate(client, server).expect("ranges overlap");
    let response = Response::ok(selected, "req-1", json!({})).expect("valid response");
    let event = Event::new(selected, event::SESSION_UPDATED, json!({})).expect("valid event");
    assert_eq!(selected.get(), 3);
    assert_eq!(response.version(), selected);
    assert_eq!(event.version(), selected);
    assert_ne!(response.version(), server.maximum());
}

#[test]
fn decimal_wire_values_are_canonical_strings_beyond_javascript_safe_integer() {
    let unsafe_in_javascript = 9_007_199_254_740_993_u64;
    let generation = RuntimeGeneration::new(unsafe_in_javascript);
    assert_eq!(
        serde_json::to_string(&generation).expect("serialize generation"),
        r#""9007199254740993""#
    );
    assert_eq!(
        serde_json::from_str::<RuntimeGeneration>(r#""9007199254740993""#)
            .expect("parse generation")
            .get(),
        unsafe_in_javascript
    );
    for invalid in [r"9007199254740993", r#""01""#, r#""+1""#, r#""""#] {
        serde_json::from_str::<RuntimeGeneration>(invalid)
            .expect_err("noncanonical decimal wire value must fail");
    }
    assert_eq!(
        serde_json::to_value(OutputOffset::new(7)).unwrap(),
        json!("7")
    );
    assert_eq!(
        serde_json::to_value(TerminalWatermark::new(8)).unwrap(),
        json!("8")
    );
    assert_eq!(
        serde_json::to_value(ReportSequence::new(9)).unwrap(),
        json!("9")
    );
    assert_eq!(
        serde_json::to_value(ProcessStartIdentity::new(10)).unwrap(),
        json!("10")
    );
}

#[cfg(feature = "ts")]
#[test]
fn decimal_wire_values_export_as_typescript_strings() {
    let config = ts_rs::Config::default();
    assert!(<RuntimeGeneration as ts_rs::TS>::decl(&config).contains("= string"));
    assert!(<OutputOffset as ts_rs::TS>::decl(&config).contains("= string"));
}

#[test]
fn session_screen_contract_has_exact_wire_shape() {
    let params = SessionScreenParams::new(SessionId("s-42".to_owned()));
    assert_eq!(
        serde_json::to_value(&params).expect("serialize screen params"),
        json!({"session_id": "s-42"})
    );
    let result = SessionScreenResult {
        session_id: SessionId("s-42".to_owned()),
        worker_id: "worker-1".to_owned(),
        runtime: SessionRuntimeIdentity::new("runtime-1", RuntimeGeneration::new(2))
            .expect("valid runtime"),
        watermark: TerminalWatermark::new(9_007_199_254_740_993),
        dimensions: TerminalDimensions::new(120, 40).expect("valid dimensions"),
        cursor: TerminalCursor {
            row: 3,
            col: 7,
            visible: true,
        },
        alternate_screen: false,
        title: Some("Build".to_owned()),
        progress: None,
        visible_lines: vec!["one".to_owned(), "two".to_owned()],
    };
    let expected = json!({
        "session_id": "s-42",
        "worker_id": "worker-1",
        "runtime_id": "runtime-1",
        "runtime_generation": "2",
        "watermark": "9007199254740993",
        "dimensions": {"cols": 120, "rows": 40},
        "cursor": {"row": 3, "col": 7, "visible": true},
        "alternate_screen": false,
        "title": "Build",
        "visible_lines": ["one", "two"]
    });
    assert_eq!(
        serde_json::to_value(&result).expect("serialize screen"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionScreenResult>(expected).expect("parse screen"),
        result
    );
    serde_json::from_value::<SessionScreenParams>(json!({
        "session_id": "s-42",
        "unknown": true
    }))
    .expect_err("unknown screen field must fail");
}

#[test]
fn session_output_contract_covers_tail_cursor_gap_and_validation() {
    let tail = SessionOutputParams::new(SessionId("s-42".to_owned()), None, None, 4096, None)
        .expect("valid tail request");
    assert_eq!(
        serde_json::to_value(&tail).expect("serialize tail request"),
        json!({"session_id": "s-42", "max_bytes": 4096})
    );
    let runtime =
        SessionRuntimeIdentity::new("runtime-1", RuntimeGeneration::new(9_007_199_254_740_993))
            .expect("valid runtime");
    let cursor = SessionOutputParams::new(
        SessionId("s-42".to_owned()),
        Some(runtime.clone()),
        Some(OutputOffset::new(9_007_199_254_740_994)),
        1024,
        Some(250),
    )
    .expect("valid cursor request");
    let cursor_json = json!({
        "session_id": "s-42",
        "runtime": {
            "runtime_id": "runtime-1",
            "runtime_generation": "9007199254740993"
        },
        "after_offset": "9007199254740994",
        "max_bytes": 1024,
        "wait_ms": 250
    });
    assert_eq!(
        serde_json::to_value(&cursor).expect("serialize cursor request"),
        cursor_json
    );
    assert_eq!(
        serde_json::from_value::<SessionOutputParams>(cursor_json).expect("parse cursor request"),
        cursor
    );

    let result = SessionOutputResult::new(
        SessionId("s-42".to_owned()),
        runtime,
        OutputOffset::new(20),
        OutputOffset::new(20),
        OutputOffset::new(24),
        OutputOffset::new(30),
        "dGVzdA==",
        Some(
            SessionOutputGap::new(OutputOffset::new(10), OutputOffset::new(20))
                .expect("valid output gap"),
        ),
        true,
        false,
    )
    .expect("valid output result");
    let value = serde_json::to_value(&result).expect("serialize output result");
    assert_eq!(
        value,
        json!({
            "session_id": "s-42",
            "runtime_id": "runtime-1",
            "runtime_generation": "9007199254740993",
            "history_start_offset": "20",
            "start_offset": "20",
            "next_offset": "24",
            "runtime_end_offset": "30",
            "data_base64": "dGVzdA==",
            "gap": {"start_offset": "10", "end_offset": "20"},
            "has_more": true,
            "timed_out": false
        })
    );
    assert_eq!(line_roundtrip(&result), result);
}

#[test]
fn session_output_params_reject_invalid_limits_and_cursor_shapes() {
    SessionOutputParams::new(
        SessionId("s-42".to_owned()),
        None,
        None,
        u32::try_from(MAX_SESSION_OUTPUT_BYTES).expect("protocol output limit fits u32"),
        None,
    )
    .expect("exact protocol output limit is valid");
    SessionOutputParams::new(
        SessionId("s-42".to_owned()),
        None,
        None,
        u32::try_from(MAX_SESSION_OUTPUT_BYTES + 1).expect("test limit fits u32"),
        None,
    )
    .expect_err("one byte above protocol output limit must fail");

    for invalid in [
        json!({"session_id":"s-42","after_offset":"1","max_bytes":1}),
        json!({"session_id":"s-42","max_bytes":0}),
        json!({"session_id":"s-42","max_bytes":1,"wait_ms":1}),
        json!({
            "session_id":"s-42",
            "runtime":{"runtime_id":"runtime-1","runtime_generation":"1"},
            "after_offset":"0",
            "max_bytes":1,
            "wait_ms": MAX_SESSION_WAIT_MS + 1
        }),
        json!({"session_id":"s-42","max_bytes":1,"unknown":true}),
    ] {
        serde_json::from_value::<SessionOutputParams>(invalid)
            .expect_err("invalid output request must fail");
    }
}

#[test]
fn session_output_result_rejects_encoding_size_and_offset_invariants() {
    let valid_session = "s-42".to_owned();
    let valid_runtime = "runtime-1".to_owned();

    assert!(matches!(
        output_result(
            valid_session.clone(),
            valid_runtime.clone(),
            "dGVzdA".to_owned(),
            0,
            0,
            4,
            4,
            None,
            false,
        ),
        Err(ObservationParamsError::InvalidOutputBase64)
    ));
    assert!(matches!(
        output_result(
            valid_session.clone(),
            valid_runtime.clone(),
            format!("{}AA==", "A".repeat(MAX_SESSION_OUTPUT_BYTES / 3 * 4)),
            0,
            0,
            u64::try_from(MAX_SESSION_OUTPUT_BYTES + 1).expect("limit fits u64"),
            u64::try_from(MAX_SESSION_OUTPUT_BYTES + 1).expect("limit fits u64"),
            None,
            false,
        ),
        Err(ObservationParamsError::OutputDataTooLarge { .. })
    ));
    assert!(matches!(
        output_result(
            valid_session.clone(),
            valid_runtime.clone(),
            String::new(),
            2,
            1,
            1,
            1,
            None,
            false,
        ),
        Err(ObservationParamsError::InvalidOutputOffsetOrder)
    ));
    assert!(matches!(
        output_result(
            valid_session.clone(),
            valid_runtime.clone(),
            "dGVzdA==".to_owned(),
            0,
            0,
            3,
            3,
            None,
            false,
        ),
        Err(ObservationParamsError::OutputLengthMismatch { .. })
    ));
}

#[test]
fn session_output_result_rejects_gap_flag_and_identifier_invariants() {
    let valid_session = "s-42".to_owned();
    let valid_runtime = "runtime-1".to_owned();

    assert!(matches!(
        SessionOutputGap::new(OutputOffset::new(2), OutputOffset::new(2)),
        Err(ObservationParamsError::InvalidOutputGap)
    ));
    assert!(matches!(
        output_result(
            valid_session.clone(),
            valid_runtime.clone(),
            String::new(),
            1,
            1,
            1,
            1,
            Some(
                SessionOutputGap::new(OutputOffset::new(0), OutputOffset::new(2))
                    .expect("standalone gap is nonempty"),
            ),
            false,
        ),
        Err(ObservationParamsError::InvalidOutputGap)
    ));
    assert!(matches!(
        output_result(
            valid_session.clone(),
            valid_runtime.clone(),
            String::new(),
            0,
            0,
            0,
            1,
            None,
            false,
        ),
        Err(ObservationParamsError::InvalidOutputHasMore)
    ));
    assert!(matches!(
        output_result(
            "s".repeat(MAX_SESSION_ID_BYTES + 1),
            valid_runtime.clone(),
            String::new(),
            0,
            0,
            0,
            0,
            None,
            false,
        ),
        Err(ObservationParamsError::IdentifierTooLong {
            field: "session_id",
            ..
        })
    ));
    assert!(matches!(
        output_result(
            valid_session,
            "r".repeat(MAX_RUNTIME_ID_BYTES + 1),
            String::new(),
            0,
            0,
            0,
            0,
            None,
            false,
        ),
        Err(ObservationParamsError::IdentifierTooLong {
            field: "runtime_id",
            ..
        })
    ));

    serde_json::from_value::<SessionOutputResult>(json!({
        "session_id": "s-42",
        "runtime_id": "runtime-1",
        "runtime_generation": "1",
        "history_start_offset": "0",
        "start_offset": "0",
        "next_offset": "0",
        "runtime_end_offset": "1",
        "data_base64": "",
        "has_more": false,
        "timed_out": false
    }))
    .expect_err("custom deserialization must enforce result invariants");
}

#[test]
fn session_wait_contract_covers_predicates_reasons_and_validation() {
    let runtime =
        SessionRuntimeIdentity::new("runtime-1", RuntimeGeneration::new(2)).expect("valid runtime");
    let params = SessionWaitParams::new(
        SessionId("s-42".to_owned()),
        Some(runtime),
        Some("2026-08-04T10:00:00Z".to_owned()),
        Some(TerminalWatermark::new(7)),
        Some(OutputOffset::new(8)),
        Some(vec![SessionState::Stopped]),
        Some(vec![AgentActivity::Blocked]),
        MAX_SESSION_WAIT_MS,
    )
    .expect("valid wait request");
    let expected = json!({
        "session_id": "s-42",
        "runtime": {"runtime_id": "runtime-1", "runtime_generation": "2"},
        "after_updated_at": "2026-08-04T10:00:00Z",
        "after_terminal_watermark": "7",
        "after_output_offset": "8",
        "states": ["stopped"],
        "activities": ["blocked"],
        "timeout_ms": MAX_SESSION_WAIT_MS
    });
    assert_eq!(
        serde_json::to_value(&params).expect("serialize wait"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionWaitParams>(expected).expect("parse wait"),
        params
    );

    let reasons = [
        (SessionWaitReason::StateMatched, "state_matched"),
        (SessionWaitReason::ActivityMatched, "activity_matched"),
        (SessionWaitReason::SessionUpdated, "session_updated"),
        (SessionWaitReason::TerminalChanged, "terminal_changed"),
        (SessionWaitReason::OutputAdvanced, "output_advanced"),
        (SessionWaitReason::RuntimeChanged, "runtime_changed"),
        (SessionWaitReason::Timeout, "timeout"),
    ];
    for (reason, wire) in reasons {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(wire));
    }
    let wait_result = SessionWaitResult {
        reason: SessionWaitReason::RuntimeChanged,
        session: running_shell_session(None),
        terminal_watermark: Some(TerminalWatermark::new(7)),
        output_offset: Some(OutputOffset::new(8)),
    };
    let session_json =
        serde_json::to_value(running_shell_session(None)).expect("serialize session");
    assert_eq!(
        serde_json::to_value(&wait_result).expect("serialize wait result"),
        json!({
            "reason": "runtime_changed",
            "session": session_json,
            "terminal_watermark": "7",
            "output_offset": "8"
        })
    );
    assert_eq!(line_roundtrip(&wait_result), wait_result);

    for invalid in [
        json!({"session_id":"s-42","states":["stopped"],"timeout_ms":0}),
        json!({"session_id":"s-42","states":["stopped"],"timeout_ms":MAX_SESSION_WAIT_MS + 1}),
        json!({"session_id":"s-42","states":[],"timeout_ms":1}),
        json!({"session_id":"s-42","activities":[],"timeout_ms":1}),
        json!({"session_id":"s-42","after_updated_at":"not-a-time","timeout_ms":1}),
        json!({"session_id":"s-42","after_output_offset":"1","timeout_ms":1}),
        json!({"session_id":"s-42","timeout_ms":1}),
        json!({"session_id":"s-42","states":["stopped"],"timeout_ms":1,"unknown":true}),
    ] {
        serde_json::from_value::<SessionWaitParams>(invalid)
            .expect_err("invalid wait request must fail");
    }
}

#[test]
fn native_report_and_error_payloads_reject_unknown_or_invalid_fields() {
    let valid = json!({
        "session_id": "s-42",
        "runtime_id": "runtime-42",
        "agent": "codex",
        "pid": 42,
        "pid_start_identity": "7",
        "sequence": "1",
        "expires_at": "2026-08-04T10:00:00Z",
        "native_session_id": "native-42"
    });
    serde_json::from_value::<SessionReportNativeIdParams>(valid.clone())
        .expect("valid native report");
    let mut unknown = valid.clone();
    unknown["secret"] = json!("must-not-be-accepted");
    serde_json::from_value::<SessionReportNativeIdParams>(unknown)
        .expect_err("unknown native report field must fail");
    let mut invalid_time = valid;
    invalid_time["expires_at"] = json!("tomorrow");
    serde_json::from_value::<SessionReportNativeIdParams>(invalid_time)
        .expect_err("invalid native report timestamp must fail");

    serde_json::from_value::<ProtocolError>(json!({
        "class": "runtime",
        "code": "session_runtime_changed",
        "msg": "redacted",
        "terminal_content": "must not enter errors"
    }))
    .expect_err("unknown error payload field must fail");
    serde_json::from_value::<Response>(json!({
        "v": 2,
        "id": "req-1",
        "err": {"class":"runtime","code":"x","msg":"redacted"},
        "data_base64": "secret"
    }))
    .expect_err("unknown response field must fail");
}

#[test]
fn notification_policy_is_strict_open_and_deterministic() {
    let flags = json!({
        "agent_blocked": true,
        "approval_required": true,
        "turn_completed": false,
        "session_finished": false,
        "error": true,
        "system": false
    });
    serde_json::from_value::<NotificationPolicy>(json!({
        "attention_dedupe_window_secs": 90,
        "attention_debounce_secs": 5,
        "enabled": flags,
        "codex": flags,
        "claude": flags
    }))
    .expect_err("legacy provider fields must fail");
    let policy: NotificationPolicy = serde_json::from_value(json!({
        "attention_dedupe_window_secs": 90,
        "attention_debounce_secs": 5,
        "enabled": flags,
        "providers": {"zeta-agent": flags, "future-agent": flags}
    }))
    .expect("arbitrary provider names remain forward compatible");
    assert!(!policy.for_provider("future-agent").system);
    assert_eq!(policy.for_provider("missing"), &policy.enabled);
    let wire = serde_json::to_string(&policy).expect("serialize policy");
    assert!(wire.find("future-agent").unwrap() < wire.find("zeta-agent").unwrap());

    let mut invalid_flags = serde_json::to_value(notification_kind_policy()).unwrap();
    invalid_flags["future_flag"] = json!(true);
    serde_json::from_value::<NotificationKindPolicy>(invalid_flags)
        .expect_err("unknown notification flag must fail");
}

#[test]
fn range_negotiation_selects_highest_overlap_and_rejects_legacy_envelope() {
    let client = ProtocolVersionRange::new(
        ProtocolVersion::new(2).expect("valid version"),
        ProtocolVersion::new(4).expect("valid version"),
    )
    .expect("ordered range");
    let daemon = ProtocolVersionRange::new(
        ProtocolVersion::new(1).expect("valid version"),
        ProtocolVersion::new(3).expect("valid version"),
    )
    .expect("ordered range");
    assert_eq!(
        negotiate(client, daemon).expect("overlap"),
        ProtocolVersion::new(3).expect("valid version")
    );
    serde_json::from_str::<Request>(
        r#"{"v":1,"id":"legacy","method":"daemon.health","params":null}"#,
    )
    .expect_err("legacy exact-version request must fail");
}

#[test]
fn native_identity_debug_redacts_sensitive_values() {
    let params = SessionReportNativeIdParams::new(
        SessionId("s-42".to_owned()),
        "runtime-42",
        "codex",
        4242,
        ProcessStartIdentity::new(7),
        ReportSequence::new(1),
        "2026-07-27T12:00:10Z",
        "secret-native-reference",
        Some("/private/transcript.jsonl".to_owned()),
    )
    .expect("valid native identity report");
    let debug = format!("{params:?}");
    assert!(!debug.contains("secret-native-reference"));
    assert!(!debug.contains("/private/transcript.jsonl"));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn m1_observation_errors_are_stable_and_payload_free() {
    let errors = [
        ProtocolError::agent_fork_unsupported(),
        ProtocolError::agent_runtime_unsupported(),
        ProtocolError::session_input_rejected(),
        ProtocolError::session_input_blocked(),
        ProtocolError::session_terminal_unavailable(),
        ProtocolError::session_has_no_managed_terminal(),
        ProtocolError::session_runtime_changed(),
        ProtocolError::session_output_limit_exceeded(),
        ProtocolError::session_wait_limit_exceeded(),
        ProtocolError::session_waiter_limit_reached(),
        ProtocolError::worker_feature_unavailable(),
    ];
    for error in errors {
        assert_eq!(error.class, ErrorClass::Runtime);
        assert!(error.recover.is_none());
        assert!(!error.msg.contains("base64"));
        assert_eq!(line_roundtrip(&error), error);
    }

    let response = Response::err(
        PROTOCOL_VERSION,
        "req-runtime-change",
        ProtocolError::session_runtime_changed(),
    )
    .expect("valid response");
    assert_eq!(
        serde_json::to_value(response).expect("serialize redacted error envelope"),
        json!({
            "v": 2,
            "id": "req-runtime-change",
            "err": {
                "class": "runtime",
                "code": "session_runtime_changed",
                "msg": "the session runtime changed; restart observation with the current runtime identity"
            }
        })
    );
}

#[test]
fn assistant_materialize_params_json_shape_roundtrips() {
    let params = AssistantMaterializeParams {
        snapshot: r#"{"sessions":[],"projects":[]}"#.to_owned(),
    };

    let value = serde_json::to_value(&params).expect("serialize params");
    assert_eq!(
        value,
        json!({
            "snapshot": r#"{"sessions":[],"projects":[]}"#
        })
    );

    let back = line_roundtrip(&params);
    assert_eq!(back, params);
}

#[test]
fn assistant_materialize_result_json_shape_roundtrips() {
    let result = AssistantMaterializeResult {
        bundle_path: "/cache/pohunek/knowledge/v1".to_owned(),
        snapshot_path: "/run/pohunek/assistant/launch-42/snapshot.json".to_owned(),
        version: "0.1.0".to_owned(),
        content_hash: "sha256:abc123".to_owned(),
        concepts: vec![ConceptMeta {
            r#type: ConceptType::Guide,
            id: "launcher".to_owned(),
            title: "Assistant launcher".to_owned(),
            description: "How the assistant launch flow uses materialized knowledge.".to_owned(),
            intents: Some(vec![ConceptIntent::Project, ConceptIntent::Help]),
            since: Some("0.1.0".to_owned()),
            changed_in: Some(vec!["0.2.0".to_owned()]),
            deprecated: Some(ConceptDeprecation::Details {
                version: "0.3.0".to_owned(),
                successor: Some("launcher-v2".to_owned()),
            }),
        }],
    };

    let value = serde_json::to_value(&result).expect("serialize result");
    assert_eq!(
        value,
        json!({
            "bundle_path": "/cache/pohunek/knowledge/v1",
            "snapshot_path": "/run/pohunek/assistant/launch-42/snapshot.json",
            "version": "0.1.0",
            "content_hash": "sha256:abc123",
            "concepts": [{
                "type": "Guide",
                "id": "launcher",
                "title": "Assistant launcher",
                "description": "How the assistant launch flow uses materialized knowledge.",
                "intents": ["project", "help"],
                "since": "0.1.0",
                "changed_in": ["0.2.0"],
                "deprecated": {
                    "version": "0.3.0",
                    "successor": "launcher-v2"
                }
            }]
        })
    );

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn concept_meta_omits_absent_optional_fields() {
    let concept = ConceptMeta {
        r#type: ConceptType::Concept,
        id: "architecture".to_owned(),
        title: "Architecture".to_owned(),
        description: "Assistant architecture overview.".to_owned(),
        intents: None,
        since: None,
        changed_in: None,
        deprecated: None,
    };

    let value = serde_json::to_value(&concept).expect("serialize concept");
    assert_eq!(
        value,
        json!({
            "type": "Concept",
            "id": "architecture",
            "title": "Architecture",
            "description": "Assistant architecture overview."
        })
    );

    let object = value.as_object().expect("concept object");
    for absent in ["intents", "since", "changed_in", "deprecated"] {
        assert!(
            !object.contains_key(absent),
            "absent {absent} must be omitted: {value}"
        );
    }

    let back = line_roundtrip(&concept);
    assert_eq!(back, concept);
}

#[test]
fn daemon_doctor_report_json_shape_roundtrips() {
    let result = DaemonDoctorResult {
        report: DoctorReport::from_checks(vec![
            DoctorCheck::new("bin:git", DoctorStatus::Ok, "found at /usr/bin/git"),
            DoctorCheck::new("bin:codex", DoctorStatus::Warn, "'codex' not found on PATH"),
        ]),
    };

    let value = serde_json::to_value(&result).expect("serialize doctor result");
    assert_eq!(
        value,
        json!({
            "report": {
                "checks": [
                    {
                        "name": "bin:git",
                        "status": "ok",
                        "detail": "found at /usr/bin/git"
                    },
                    {
                        "name": "bin:codex",
                        "status": "warn",
                        "detail": "'codex' not found on PATH"
                    }
                ],
                "overall": "ok"
            }
        })
    );

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn agent_runtime_json_shape_roundtrips_with_path() {
    let runtime = AgentRuntime {
        agent: "codex".to_owned(),
        agent_base: Some(AgentKind::Codex),
        available: true,
        path: Some("/usr/local/bin/codex".to_owned()),
        version: None,
        supported: None,
    };

    let value = serde_json::to_value(&runtime).expect("serialize agent runtime");
    assert_eq!(
        value,
        json!({
            "agent": "codex",
            "agent_base": "codex",
            "available": true,
            "path": "/usr/local/bin/codex"
        })
    );

    let back = line_roundtrip(&runtime);
    assert_eq!(back, runtime);
}

#[test]
fn agent_runtime_omits_absent_path() {
    let runtime = AgentRuntime {
        agent: "shell".to_owned(),
        agent_base: Some(AgentKind::Shell),
        available: true,
        path: None,
        version: None,
        supported: None,
    };

    let value = serde_json::to_value(&runtime).expect("serialize agent runtime");
    assert_eq!(
        value,
        json!({
            "agent": "shell",
            "agent_base": "shell",
            "available": true
        })
    );
    assert!(
        !value
            .as_object()
            .expect("runtime object")
            .contains_key("path"),
        "absent path must be omitted: {value}"
    );

    let back = line_roundtrip(&runtime);
    assert_eq!(back, runtime);
}

#[test]
fn agent_runtime_roundtrips_optional_version_support() {
    let runtime = AgentRuntime {
        agent: "hermes".to_owned(),
        agent_base: Some(AgentKind::Hermes),
        available: true,
        path: Some("/usr/local/bin/hermes".to_owned()),
        version: Some("0.20.0".to_owned()),
        supported: Some(true),
    };

    assert_eq!(
        serde_json::to_value(&runtime).expect("serialize Hermes runtime"),
        json!({
            "agent": "hermes",
            "agent_base": "hermes",
            "available": true,
            "path": "/usr/local/bin/hermes",
            "version": "0.20.0",
            "supported": true
        })
    );
    assert_eq!(line_roundtrip(&runtime), runtime);

    let legacy: AgentRuntime = serde_json::from_value(json!({
        "agent": "hermes",
        "available": true,
        "path": "/usr/local/bin/hermes"
    }))
    .expect("older runtime inventory remains readable");
    assert_eq!(legacy.version, None);
    assert_eq!(legacy.supported, None);
    assert_eq!(legacy.agent_base, None);
}

#[test]
fn host_capabilities_json_shape_roundtrips() {
    let caps = HostCapabilities {
        daemon_version: "0.1.0".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        supported_agents: vec![
            "shell".to_owned(),
            "codex".to_owned(),
            "claude".to_owned(),
            "hermes".to_owned(),
        ],
        runtimes: vec![
            AgentRuntime {
                agent: "shell".to_owned(),
                agent_base: Some(AgentKind::Shell),
                available: true,
                path: None,
                version: None,
                supported: None,
            },
            AgentRuntime {
                agent: "codex".to_owned(),
                agent_base: Some(AgentKind::Codex),
                available: true,
                path: Some("/usr/local/bin/codex".to_owned()),
                version: None,
                supported: None,
            },
            AgentRuntime {
                agent: "claude".to_owned(),
                agent_base: Some(AgentKind::Claude),
                available: false,
                path: None,
                version: None,
                supported: None,
            },
            AgentRuntime {
                agent: "hermes".to_owned(),
                agent_base: Some(AgentKind::Hermes),
                available: true,
                path: Some("/usr/local/bin/hermes".to_owned()),
                version: Some("0.20.0".to_owned()),
                supported: Some(true),
            },
        ],
        git_available: true,
        worktree_supported: true,
        terminal_read_supported: true,
        output_read_supported: true,
        session_wait_supported: true,
    };

    let value = serde_json::to_value(&caps).expect("serialize host capabilities");
    assert_eq!(
        value,
        json!({
            "daemon_version": "0.1.0",
            "protocol_version": PROTOCOL_VERSION,
            "supported_agents": ["shell", "codex", "claude", "hermes"],
            "runtimes": [
                { "agent": "shell", "agent_base": "shell", "available": true },
                { "agent": "codex", "agent_base": "codex", "available": true, "path": "/usr/local/bin/codex" },
                { "agent": "claude", "agent_base": "claude", "available": false },
                {
                    "agent": "hermes",
                    "agent_base": "hermes",
                    "available": true,
                    "path": "/usr/local/bin/hermes",
                    "version": "0.20.0",
                    "supported": true
                }
            ],
            "git_available": true,
            "worktree_supported": true,
            "terminal_read_supported": true,
            "output_read_supported": true,
            "session_wait_supported": true
        })
    );

    let back = line_roundtrip(&caps);
    assert_eq!(back, caps);
}

#[test]
fn host_capabilities_ignores_unknown_fields_for_additive_evolution() {
    // A newer host may add capability fields; an older peer must still parse it.
    let raw = r#"{
        "daemon_version": "0.2.0",
        "protocol_version": 2,
        "supported_agents": ["shell"],
        "runtimes": [],
        "git_available": false,
        "worktree_supported": false,
        "terminal_read_supported": false,
        "output_read_supported": false,
        "session_wait_supported": false,
        "future_capability": true
    }"#;
    let caps: HostCapabilities = serde_json::from_str(raw).expect("must ignore unknown fields");
    assert_eq!(caps.daemon_version, "0.2.0");
    assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
    assert!(!caps.git_available);
    assert!(caps.runtimes.is_empty());
}

#[test]
fn netbird_cli_missing_has_discovery_class_stable_code_and_recover() {
    let err = ProtocolError::netbird_cli_missing();
    assert_eq!(err.class, ErrorClass::Discovery);
    assert_eq!(err.code, "netbird_cli_missing");
    assert!(
        err.recover.is_some(),
        "missing CLI should suggest a recovery: {err:?}"
    );
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn netbird_state_unavailable_has_discovery_class_stable_code_and_detail() {
    let err = ProtocolError::netbird_state_unavailable("daemon not running");
    assert_eq!(err.class, ErrorClass::Discovery);
    assert_eq!(err.code, "netbird_state_unavailable");
    assert!(
        err.msg.contains("daemon not running"),
        "message must carry the detail: {}",
        err.msg
    );
    assert!(
        err.recover.is_some(),
        "unavailable state should suggest a recovery: {err:?}"
    );
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn host_unknown_has_discovery_class_stable_code_and_names_host() {
    let err = ProtocolError::host_unknown("build-box");
    assert_eq!(err.class, ErrorClass::Discovery);
    assert_eq!(err.code, "host_unknown");
    assert!(
        err.msg.contains("build-box"),
        "message must name the host: {}",
        err.msg
    );
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn host_unreachable_has_transport_class_stable_code_names_host_and_recover() {
    let err = ProtocolError::host_unreachable("build-box");
    assert_eq!(err.class, ErrorClass::Transport);
    assert_eq!(err.code, "host_unreachable");
    assert!(
        err.msg.contains("build-box"),
        "message must name the host: {}",
        err.msg
    );
    assert!(
        err.recover.is_some(),
        "unreachable host should suggest a recovery: {err:?}"
    );
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn remote_daemon_unavailable_has_daemon_class_stable_code_and_names_host() {
    let err = ProtocolError::remote_daemon_unavailable("build-box");
    assert_eq!(err.class, ErrorClass::Daemon);
    assert_eq!(err.code, "remote_daemon_unavailable");
    assert!(
        err.msg.contains("build-box"),
        "message must name the host: {}",
        err.msg
    );
    let back = line_roundtrip(&err);
    assert_eq!(back, err);
}

#[test]
fn new_remote_error_codes_are_distinct() {
    // Every milestone-11 error code (plus the reused version_mismatch) must be a
    // distinct, stable string so `--json` consumers can branch on it.
    let codes = [
        ProtocolError::netbird_cli_missing().code,
        ProtocolError::netbird_state_unavailable("x").code,
        ProtocolError::host_unknown("h").code,
        ProtocolError::host_unreachable("h").code,
        ProtocolError::remote_daemon_unavailable("h").code,
        ProtocolError::version_mismatch(
            ProtocolVersionRange::new(
                ProtocolVersion::new(1).expect("valid version"),
                ProtocolVersion::new(1).expect("valid version"),
            )
            .expect("ordered range"),
            ProtocolVersionRange::new(
                ProtocolVersion::new(2).expect("valid version"),
                ProtocolVersion::new(2).expect("valid version"),
            )
            .expect("ordered range"),
        )
        .code,
    ];
    let unique: std::collections::HashSet<&str> = codes.iter().map(String::as_str).collect();
    assert_eq!(
        unique.len(),
        codes.len(),
        "error codes must all be distinct: {codes:?}"
    );
}

#[test]
fn assistant_error_codes_have_expected_classes_and_recovery() {
    let no_agent = ProtocolError::no_capable_agent();
    assert_eq!(no_agent.class, ErrorClass::Runtime);
    assert_eq!(no_agent.code, "no_capable_agent");
    assert!(no_agent.recover.is_some());

    let unavailable = ProtocolError::bundle_unavailable("/cache/knowledge");
    assert_eq!(unavailable.class, ErrorClass::Runtime);
    assert_eq!(unavailable.code, "bundle_unavailable");
    assert!(unavailable.msg.contains("/cache/knowledge"));

    let materialization = ProtocolError::materialization_failed("/cache/knowledge", "denied");
    assert_eq!(materialization.class, ErrorClass::Runtime);
    assert_eq!(materialization.code, "materialization_failed");
    assert!(materialization.msg.contains("denied"));

    let unreadable = ProtocolError::agent_cannot_read_bundle("/cache/knowledge", "sandbox");
    assert_eq!(unreadable.class, ErrorClass::Runtime);
    assert_eq!(unreadable.code, "agent_cannot_read_bundle");
    assert!(unreadable.msg.contains("sandbox"));

    let unsupported = ProtocolError::assistant_method_unsupported("assistant.materialize");
    assert_eq!(unsupported.class, ErrorClass::Daemon);
    assert_eq!(unsupported.code, "assistant_method_unsupported");
    assert!(unsupported.recover.is_some());
}

#[test]
fn assistant_error_codes_are_distinct() {
    let codes = [
        ProtocolError::no_capable_agent().code,
        ProtocolError::bundle_unavailable("p").code,
        ProtocolError::materialization_failed("p", "e").code,
        ProtocolError::agent_cannot_read_bundle("p", "c").code,
        ProtocolError::assistant_method_unsupported("m").code,
    ];
    let unique: std::collections::HashSet<&str> = codes.iter().map(String::as_str).collect();
    assert_eq!(
        unique.len(),
        codes.len(),
        "assistant error codes must all be distinct: {codes:?}"
    );
}

#[test]
fn project_prompt_json_shape_roundtrips() {
    let result = protocol::ProjectPromptResult {
        name: "issue".to_owned(),
        content: "Process ${title}\n\n${body}".to_owned(),
        layer: protocol::PromptLayer::InRepo,
    };
    assert_eq!(line_roundtrip(&result), result);

    // PromptLayer serializes in snake_case, the contract the two clients share.
    let value: Value = serde_json::to_value(&result).expect("to_value");
    assert_eq!(value["layer"], json!("in_repo"));
    assert_eq!(
        serde_json::to_value(protocol::PromptLayer::Host).expect("to_value"),
        json!("host")
    );

    let params = protocol::ProjectPromptParams {
        reference: "ui".to_owned(),
        name: "issue".to_owned(),
    };
    assert_eq!(line_roundtrip(&params), params);
}

#[test]
fn provider_kind_json_shape_roundtrips() {
    let cases = [
        (protocol::ProviderKind::LinearIssue, json!("linear_issue")),
        (protocol::ProviderKind::GithubPr, json!("github_pr")),
        (protocol::ProviderKind::None, json!("none")),
    ];

    for (provider, expected) in cases {
        let value = serde_json::to_value(&provider).expect("serialize provider");
        assert_eq!(value, expected);

        let back = line_roundtrip(&provider);
        assert_eq!(back, provider);
    }
}

#[test]
fn project_action_json_shape_roundtrips() {
    let result = protocol::ProjectActionResult {
        provider: protocol::ProviderKind::GithubPr,
        agent: "codex".to_owned(),
        base_branch: Some("main".to_owned()),
        branch: None,
        prompt_name: "pr-review".to_owned(),
        prompt_content: "Review ${title}\n\n${body}".to_owned(),
    };
    assert_eq!(line_roundtrip(&result), result);

    let value: Value = serde_json::to_value(&result).expect("to_value");
    assert_eq!(
        value,
        json!({
            "provider": "github_pr",
            "agent": "codex",
            "base_branch": "main",
            "prompt_name": "pr-review",
            "prompt_content": "Review ${title}\n\n${body}"
        })
    );

    let params = protocol::ProjectActionParams {
        reference: "ui".to_owned(),
        name: "review-pr".to_owned(),
    };
    assert_eq!(line_roundtrip(&params), params);
}

#[test]
fn project_action_omits_absent_optional_fields() {
    let result = protocol::ProjectActionResult {
        provider: protocol::ProviderKind::None,
        agent: "shell".to_owned(),
        base_branch: None,
        branch: Some("feature/static".to_owned()),
        prompt_name: "task".to_owned(),
        prompt_content: "Do the task".to_owned(),
    };

    let value: Value = serde_json::to_value(&result).expect("to_value");
    assert!(
        !value
            .as_object()
            .expect("object")
            .contains_key("base_branch"),
        "absent base_branch must be omitted: {value}"
    );
    assert_eq!(value["branch"], json!("feature/static"));

    let back = line_roundtrip(&result);
    assert_eq!(back, result);
}

#[test]
fn action_summary_json_shape_roundtrips() {
    let summary = protocol::ActionSummary {
        name: "issue".to_owned(),
        provider: protocol::ProviderKind::LinearIssue,
        template: "linear".to_owned(),
        layer: protocol::PromptLayer::Host,
    };
    assert_eq!(line_roundtrip(&summary), summary);

    let value: Value = serde_json::to_value(&summary).expect("to_value");
    assert_eq!(
        value,
        json!({
            "name": "issue",
            "provider": "linear_issue",
            "template": "linear",
            "layer": "host"
        })
    );

    let result = protocol::ProjectActionsResult {
        actions: vec![summary],
    };
    assert_eq!(line_roundtrip(&result), result);

    let params = protocol::ProjectActionsParams {
        reference: "ui".to_owned(),
    };
    assert_eq!(line_roundtrip(&params), params);
}

#[test]
fn typed_method_markers_pair_method_params_and_results() {
    fn assert_contract<M, Params, Output>(name: &str)
    where
        M: protocol::Method<Params = Params, Output = Output>,
    {
        assert_eq!(M::NAME, name);
    }

    assert_contract::<protocol::method::DaemonHealth, (), protocol::DaemonHealthResult>(
        protocol::method::DAEMON_HEALTH,
    );
    assert_contract::<
        protocol::method::SessionNew,
        protocol::SessionNewParams,
        protocol::SessionNewResult,
    >(protocol::method::SESSION_NEW);
    assert_contract::<protocol::method::SessionInspect, protocol::SessionId, protocol::SessionInfo>(
        protocol::method::SESSION_INSPECT,
    );
    assert_contract::<
        protocol::method::HostDiscover,
        protocol::HostDiscoverParams,
        Vec<protocol::HostRecord>,
    >(protocol::method::HOST_DISCOVER);
    assert_contract::<
        protocol::method::ProjectAction,
        protocol::ProjectActionParams,
        protocol::ProjectActionResult,
    >(protocol::method::PROJECT_ACTION);
    assert_contract::<
        protocol::method::WorktreeRemove,
        protocol::WorktreeRemoveParams,
        protocol::WorktreeRemoveResult,
    >(protocol::method::WORKTREE_REMOVE);
}
