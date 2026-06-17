//! Round-trip serialize/deserialize tests and version-negotiation tests for the
//! control protocol (milestone 1 checkpoint: "round-trip serde unit tests pass").

use protocol::{
    method, negotiate, ErrorClass, Event, ProtocolError, ProtocolVersion, Request, Response,
    StateSource, PROTOCOL_VERSION,
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
    let resp = Response::ok("req-7f3", json!({ "session_id": "s-42", "state": "working" }));
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
fn event_roundtrip() {
    let event = Event::new(
        "agent_state",
        json!({
            "session_id": "s-42",
            "state": "blocked",
            "source": StateSource::OscTitle,
            "ts": "2026-06-17T10:00:00Z"
        }),
    );
    let back = line_roundtrip(&event);
    assert_eq!(event, back);
    assert_eq!(back.v, PROTOCOL_VERSION);
    assert_eq!(back.event, "agent_state");
    // Payload keys are flattened to the top level of the JSON object.
    let line = serde_json::to_string(&event).expect("serialize");
    assert!(line.contains("\"session_id\":\"s-42\""), "line: {line}");
    assert!(line.contains("\"source\":\"osc_title\""), "line: {line}");
}

#[test]
fn event_with_id_roundtrip() {
    let event = Event::new("session_exit", json!({ "session_id": "s-7", "exit_code": 0 }))
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
fn protocol_version_serializes_as_bare_integer() {
    // The `v` field must be a plain integer on the wire, not an object.
    let line = serde_json::to_string(&PROTOCOL_VERSION).expect("serialize");
    assert_eq!(line, "1");
}
