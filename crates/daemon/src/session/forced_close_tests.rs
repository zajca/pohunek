//! The forced-output-close marker of a terminal session.

// Rust guideline compliant 2026-10-02

use super::{note_output_force_closed, ObservedExit};
use pohunek_worker_protocol::ExitStatus;

fn status(code: Option<i32>, signal: Option<i32>, output_forced_closed: bool) -> ExitStatus {
    ExitStatus {
        code,
        signal,
        stopped_by_user: true,
        exited_at_ms: 1,
        output_forced_closed,
    }
}

#[test]
fn marking_a_session_is_idempotent_and_leaves_its_warnings_alone() {
    let mut info: protocol::SessionInfo = serde_json::from_value(serde_json::json!({
        "id": "s-1",
        "agent": "shell",
        "agent_base": "shell",
        "cwd": "/work",
        "pid": 1,
        "cols": 80,
        "rows": 24,
        "state": "stopped",
        "state_source": "process",
        "created_at": "2026-10-02T00:00:00Z",
        "updated_at": "2026-10-02T00:00:00Z",
    }))
    .expect("minimal session info");
    assert!(!info.output_force_closed);
    note_output_force_closed(&mut info);
    note_output_force_closed(&mut info);
    assert!(info.output_force_closed);
    assert!(info.warnings.is_empty());
}

#[test]
fn a_terminal_status_carries_its_exit_and_the_forced_close_flag() {
    let forced = ObservedExit::from_status(&status(None, Some(15), true));
    assert!(forced.output_forced_closed);
    assert_eq!(forced.exit.exit_code, None);
    assert!(!forced.exit.success, "a signal death is never a success");

    let clean = ObservedExit::from_status(&status(Some(0), None, false));
    assert!(!clean.output_forced_closed);
    assert!(clean.exit.success);
}
