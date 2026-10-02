//! The forced-output-close marker of a terminal session.

// Rust guideline compliant 2026-10-02

use pohunek_worker_protocol::ExitStatus;
use protocol::SessionWarningKind;

use super::{note_output_force_closed, ObservedExit};

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
fn the_warning_is_added_once() {
    let mut warnings = Vec::new();
    note_output_force_closed(&mut warnings);
    note_output_force_closed(&mut warnings);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].kind, SessionWarningKind::OutputForceClosed);
    assert!(warnings[0].message.contains("force-closed"));
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
