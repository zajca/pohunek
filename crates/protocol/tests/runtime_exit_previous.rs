//! A runtime that carries an `exit`, read by the previous release's types.
//!
//! `PreviousSessionRuntime` copies the shape release 0.35 shipped, so the
//! expectation does not come from the code under test. A 0.35 client decodes
//! the new payload and ignores `exit`; a current client decodes a 0.35 payload
//! as a runtime without one, and tolerates a cause or a field it does not know.
//! `Release038SessionRuntime` copies the shape release 0.38 shipped (the
//! `exit` of three keys), which must decode a payload with every later key.

// Rust guideline compliant 2026-10-20

use protocol::{RuntimeExit, RuntimeGeneration, RuntimeState, SessionRuntime};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PreviousRuntimeState {
    Starting,
    Live,
    Reconnecting,
    Terminal,
    Lost,
    Conflict,
    Incompatible,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct PreviousSessionRuntime {
    state: PreviousRuntimeState,
    runtime_generation: RuntimeGeneration,
    #[serde(default)]
    worker_id: Option<String>,
    #[serde(default)]
    worker_instance_id: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    last_connected_at: Option<String>,
    #[serde(default)]
    loss_reason: Option<String>,
}

/// A previous-release result that rejects unknown fields at its own level
/// and nests a runtime, the way `SessionWaitResult` nests a session.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousStrictWrapper {
    runtime: PreviousSessionRuntime,
}

fn lost_runtime(exit: Option<RuntimeExit>) -> SessionRuntime {
    SessionRuntime {
        state: RuntimeState::Lost,
        runtime_generation: RuntimeGeneration::new(1),
        worker_id: Some("w-1".to_owned()),
        worker_instance_id: Some("wi-1".to_owned()),
        started_at: Some("2026-01-01T00:00:00Z".to_owned()),
        last_connected_at: Some("2026-01-01T00:00:01Z".to_owned()),
        loss_reason: Some("runtime_lost".to_owned()),
        exit,
        loss_cause: None,
    }
}

fn oom_kill() -> RuntimeExit {
    RuntimeExit {
        cause: "oom_kill".to_owned(),
        signal: Some(15),
        code: None,
        oom_killer: None,
        memory_peak_bytes: None,
        ended_at: None,
    }
}

#[test]
fn the_previous_release_decodes_a_runtime_with_an_exit() {
    let wire = serde_json::to_value(lost_runtime(Some(oom_kill()))).expect("serialize runtime");
    assert_eq!(wire["exit"], json!({"cause": "oom_kill", "signal": 15}));

    let previous: PreviousSessionRuntime =
        serde_json::from_value(wire.clone()).expect("the previous release decodes the runtime");
    assert_eq!(previous.state, PreviousRuntimeState::Lost);
    assert_eq!(previous.loss_reason.as_deref(), Some("runtime_lost"));
    assert_eq!(previous.worker_instance_id.as_deref(), Some("wi-1"));

    let wrapped: PreviousStrictWrapper = serde_json::from_value(json!({"runtime": wire}))
        .expect("a strict wrapper still accepts the nested runtime");
    assert_eq!(wrapped.runtime, previous);
}

#[test]
fn a_runtime_without_an_exit_serializes_as_before() {
    let wire = serde_json::to_value(lost_runtime(None)).expect("serialize runtime");
    assert!(wire.get("exit").is_none(), "{wire}");
}

#[test]
fn the_current_type_decodes_a_previous_release_runtime_as_one_without_an_exit() {
    let previous = json!({
        "state": "lost",
        "runtime_generation": "1",
        "worker_id": "w-1",
        "worker_instance_id": "wi-1",
        "started_at": "2026-01-01T00:00:00Z",
        "last_connected_at": "2026-01-01T00:00:01Z",
        "loss_reason": "runtime_lost"
    });
    let runtime: SessionRuntime =
        serde_json::from_value(previous).expect("the current type decodes the old payload");
    assert_eq!(runtime, lost_runtime(None));
}

#[test]
fn the_current_type_tolerates_an_unknown_cause_and_an_unknown_field() {
    let wire = json!({
        "state": "lost",
        "runtime_generation": "1",
        "exit": {"cause": "from_a_later_release", "code": 7, "since": "later"}
    });
    let runtime: SessionRuntime =
        serde_json::from_value(wire).expect("an open cause and a later field are tolerated");
    assert_eq!(
        runtime.exit,
        Some(RuntimeExit {
            cause: "from_a_later_release".to_owned(),
            signal: None,
            code: Some(7),
            oom_killer: None,
            memory_peak_bytes: None,
            ended_at: None,
        })
    );
}

/// The `exit` shape release 0.38 shipped.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct Release038Exit {
    cause: String,
    #[serde(default)]
    signal: Option<i32>,
    #[serde(default)]
    code: Option<i32>,
}

/// The runtime shape release 0.38 shipped.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct Release038SessionRuntime {
    state: PreviousRuntimeState,
    runtime_generation: RuntimeGeneration,
    #[serde(default)]
    loss_reason: Option<String>,
    #[serde(default)]
    exit: Option<Release038Exit>,
}

/// A 0.38 result that rejects unknown fields at its own level and nests a
/// runtime, the way `SessionWaitResult` nests a session.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Release038StrictWrapper {
    runtime: Release038SessionRuntime,
}

#[test]
fn release_038_decodes_every_later_exit_and_loss_key() {
    let mut exit = oom_kill();
    exit.oom_killer = Some("systemd_oomd".to_owned());
    exit.memory_peak_bytes = Some(29_200_000_000);
    exit.ended_at = Some("2026-11-01T08:39:00Z".to_owned());
    let with_exit = serde_json::to_value(lost_runtime(Some(exit))).expect("serialize runtime");
    assert_eq!(
        with_exit["exit"],
        json!({
            "cause": "oom_kill",
            "signal": 15,
            "oom_killer": "systemd_oomd",
            "memory_peak_bytes": 29_200_000_000_u64,
            "ended_at": "2026-11-01T08:39:00Z"
        })
    );
    let mut lost = lost_runtime(None);
    lost.loss_cause = Some("user_manager_lost".to_owned());
    let with_cause = serde_json::to_value(lost).expect("serialize runtime");
    assert_eq!(with_cause["loss_cause"], "user_manager_lost");

    for wire in [with_exit, with_cause] {
        let decoded: Release038SessionRuntime = serde_json::from_value(wire.clone())
            .expect("release 0.38 decodes the runtime and ignores the later keys");
        assert_eq!(decoded.state, PreviousRuntimeState::Lost);
        let wrapped: Release038StrictWrapper = serde_json::from_value(json!({"runtime": wire}))
            .expect("a strict 0.38 wrapper accepts the nested runtime");
        assert_eq!(wrapped.runtime, decoded);
    }
}

#[test]
fn a_runtime_without_a_loss_cause_serializes_without_the_key() {
    let wire = serde_json::to_value(lost_runtime(None)).expect("serialize runtime");
    assert!(wire.get("loss_cause").is_none(), "{wire}");
    let decoded: SessionRuntime = serde_json::from_value(json!({
        "state": "lost",
        "runtime_generation": "1",
        "loss_cause": "host_restarted"
    }))
    .expect("a cause of a documented value decodes");
    assert_eq!(decoded.loss_cause.as_deref(), Some("host_restarted"));
}
