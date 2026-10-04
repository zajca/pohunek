//! The private worker wire keeps the key `runtime_id` for the worker instance
//! identifier, so a worker started by an earlier daemon build and a newer
//! daemon parse each other's messages.

// Rust guideline compliant 2026-10-04

use pohunek_worker_protocol::{
    ControlEvent, EventKind, FrameHeader, FrameKind, LeaseId, ProcessIdentity, ResponseKind,
    RuntimeScope, SessionId, StreamId, WorkerId, WorkerInstanceId, CURRENT_VERSION,
};
use serde_json::{json, Value};

fn instance() -> WorkerInstanceId {
    WorkerInstanceId::new("instance-1").expect("valid worker instance id")
}

fn assert_runtime_id_key(value: &Value) {
    assert_eq!(value["runtime_id"], "instance-1", "{value}");
    assert!(value.get("worker_instance_id").is_none(), "{value}");
}

#[test]
fn runtime_scope_uses_the_runtime_id_key() {
    let scope = RuntimeScope {
        lease_id: LeaseId::new("lease-1").expect("valid lease"),
        session_id: SessionId::new("s-1").expect("valid session"),
        worker_id: WorkerId::new("w-1").expect("valid worker"),
        worker_instance_id: instance(),
    };

    let value = serde_json::to_value(&scope).expect("serialize scope");

    assert_runtime_id_key(&value);
    let decoded: RuntimeScope = serde_json::from_value(json!({
        "lease_id": "lease-1",
        "session_id": "s-1",
        "worker_id": "w-1",
        "runtime_id": "instance-1",
    }))
    .expect("decode scope from the wire key");
    assert_eq!(decoded, scope);
}

#[test]
fn initialized_response_and_events_use_the_runtime_id_key() {
    let child_process = ProcessIdentity {
        pid: 7,
        start_identity: 9,
    };
    let response = ResponseKind::Initialized {
        worker_instance_id: instance(),
        child_process,
    };
    assert_runtime_id_key(&serde_json::to_value(&response).expect("serialize response"));

    let event = ControlEvent {
        event_sequence: 1,
        kind: EventKind::OutputAdvanced {
            worker_instance_id: instance(),
            next_offset: 4,
        },
    };
    assert_runtime_id_key(&serde_json::to_value(&event).expect("serialize event"));
}

#[test]
fn data_frame_header_uses_the_runtime_id_key() {
    let header = FrameHeader {
        version: CURRENT_VERSION,
        stream_id: StreamId::new("stream-1").expect("valid stream"),
        worker_instance_id: instance(),
        kind: FrameKind::Output { offset: 0 },
    };

    let value = serde_json::to_value(&header).expect("serialize header");

    assert_runtime_id_key(&value);
    let decoded: FrameHeader = serde_json::from_value(value).expect("decode header");
    assert_eq!(decoded, header);
}

#[test]
fn the_new_spelling_is_not_accepted_on_the_wire() {
    let error = serde_json::from_value::<RuntimeScope>(json!({
        "lease_id": "lease-1",
        "session_id": "s-1",
        "worker_id": "w-1",
        "worker_instance_id": "instance-1",
    }))
    .expect_err("only the `runtime_id` key is read");

    assert!(error.to_string().contains("runtime_id"), "{error}");
}
