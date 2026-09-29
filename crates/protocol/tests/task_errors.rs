//! Wire-contract tests for the delegated task layer errors (task RFC section
//! 13.1): stable codes, classes, recovery hints, payload-free text and JSON
//! round trips.

use std::collections::HashSet;

use protocol::{ErrorClass, ProtocolError, Response, PROTOCOL_VERSION};
use serde_json::json;

/// Code prefixes admitted for task-layer errors.
///
/// `task_` for task methods, `worktree_` for the shared-worktree fence and
/// removal refusals raised by session methods, `check_` for check cleanup.
const TASK_CODE_PREFIXES: [&str; 3] = ["task_", "worktree_", "check_"];

/// Codes that exist outside the task layer and must never be reused by it.
const PRE_TASK_CODES: [&str; 6] = [
    "worktree_branch_in_use",
    "worktree_path_conflict",
    "worktree_store_error",
    "worktree_add_failed",
    "session_waiter_limit_reached",
    "agent_fork_unsupported",
];

/// Constructor, expected code, expected class and whether a recovery hint is set.
type Case = (fn() -> ProtocolError, &'static str, ErrorClass, bool);

/// Every task-layer error constructor with its expected wire identity.
const TASK_ERRORS: [Case; 34] = [
    (
        ProtocolError::task_turn_open,
        "task_turn_open",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_attention_open,
        "task_attention_open",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_agent_busy,
        "task_agent_busy",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_worktree_busy,
        "task_worktree_busy",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::worktree_busy,
        "worktree_busy",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_worktree_unavailable,
        "task_worktree_unavailable",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_worktree_mode_conflict,
        "task_worktree_mode_conflict",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_session_ended,
        "task_session_ended",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_session_unavailable,
        "task_session_unavailable",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_turn_queued,
        "task_turn_queued",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_worktree_via_investigate,
        "task_worktree_via_investigate",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_turn_ceiling_reached,
        "task_turn_ceiling_reached",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_answer_unsupported,
        "task_answer_unsupported",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_answer_unverifiable,
        "task_answer_unverifiable",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_payload_mismatch,
        "task_payload_mismatch",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_stop_precondition_failed,
        "task_stop_precondition_failed",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::worktree_users_changed,
        "worktree_users_changed",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_attention_stale,
        "task_attention_stale",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_result_unknown,
        "task_result_unknown",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_snapshot_retired,
        "task_snapshot_retired",
        ErrorClass::Runtime,
        false,
    ),
    (
        ProtocolError::task_cursor_expired,
        "task_cursor_expired",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::worktree_in_use,
        "worktree_in_use",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_result_pending,
        "task_result_pending",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_check_unconfined,
        "task_check_unconfined",
        ErrorClass::Configuration,
        true,
    ),
    (
        ProtocolError::task_check_not_permitted,
        "task_check_not_permitted",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_store_full,
        "task_store_full",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_review_limit_reached,
        "task_review_limit_reached",
        ErrorClass::Runtime,
        false,
    ),
    (
        ProtocolError::task_waiter_limit_reached,
        "task_waiter_limit_reached",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_fingerprint_key_missing,
        "task_fingerprint_key_missing",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_request_conflict,
        "task_request_conflict",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_investigate_no_checks,
        "task_investigate_no_checks",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_fork_unsupported,
        "task_fork_unsupported",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::task_investigate_unsupported,
        "task_investigate_unsupported",
        ErrorClass::Runtime,
        true,
    ),
    (
        ProtocolError::check_cleanup_stuck,
        "check_cleanup_stuck",
        ErrorClass::Runtime,
        true,
    ),
];

/// One expected task-layer error: constructor output, code, class, hint presence.
struct Expected {
    error: ProtocolError,
    code: &'static str,
    class: ErrorClass,
    has_recover: bool,
}

/// Builds every task-layer error once from [`TASK_ERRORS`].
fn task_errors() -> Vec<Expected> {
    TASK_ERRORS
        .iter()
        .map(|&(build, code, class, has_recover)| Expected {
            error: build(),
            code,
            class,
            has_recover,
        })
        .collect()
}

/// Asserts that fixed error text carries no caller payload.
///
/// Task errors never echo prompts, answers, paths, ids or secrets, so their
/// text is short single-line prose with no path separators, digits, quotes or
/// formatting placeholders.
fn assert_payload_free(code: &str, text: &str) {
    assert!(!text.is_empty(), "{code}: empty text");
    assert!(text.len() <= 160, "{code}: text longer than one short line");
    assert!(text.is_ascii(), "{code}: non-ASCII text");
    for forbidden in ['/', '\\', '"', '`', '{', '}', '\n', '\r', '\t', ':'] {
        assert!(
            !text.contains(forbidden),
            "{code}: text contains {forbidden:?}"
        );
    }
    assert!(
        !text.chars().any(|c| c.is_ascii_digit()),
        "{code}: text contains a digit"
    );
}

fn is_snake_case(code: &str) -> bool {
    !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !code.starts_with('_')
        && !code.ends_with('_')
        && !code.contains("__")
}

#[test]
fn task_errors_have_stable_codes_classes_and_hints() {
    for case in task_errors() {
        assert_eq!(case.error.code, case.code);
        assert_eq!(case.error.class, case.class, "{}: class", case.code);
        assert_eq!(
            case.error.recover.is_some(),
            case.has_recover,
            "{}: recovery hint presence",
            case.code
        );
    }
}

#[test]
fn task_error_codes_are_unique_and_follow_the_naming_rule() {
    let cases = task_errors();
    let codes: HashSet<&str> = cases.iter().map(|case| case.code).collect();
    assert_eq!(
        codes.len(),
        cases.len(),
        "task error codes must be distinct"
    );
    for code in &codes {
        assert!(is_snake_case(code), "{code}: not lowercase snake_case");
        assert!(
            TASK_CODE_PREFIXES
                .iter()
                .any(|prefix| code.starts_with(prefix)),
            "{code}: missing a task-layer prefix"
        );
        assert!(
            !PRE_TASK_CODES.contains(code),
            "{code}: collides with an existing code"
        );
    }
}

#[test]
fn task_error_text_is_payload_free() {
    for case in task_errors() {
        assert_payload_free(case.code, &case.error.msg);
        if let Some(recover) = &case.error.recover {
            assert_payload_free(case.code, recover);
        }
    }
}

#[test]
fn task_errors_round_trip_as_json() {
    for case in task_errors() {
        let line = serde_json::to_string(&case.error).expect("serialize task error");
        assert!(!line.contains('\n'), "{}: multi-line JSON", case.code);
        let back: ProtocolError = serde_json::from_str(&line).expect("deserialize task error");
        assert_eq!(back, case.error);

        let value = serde_json::to_value(&case.error).expect("task error as value");
        assert_eq!(value["code"], case.code);
        assert_eq!(value["class"], case.class.to_string());
        assert_eq!(
            value.get("recover").is_some(),
            case.has_recover,
            "{}",
            case.code
        );
    }
}

#[test]
fn task_error_wire_shape_is_exact() {
    assert_eq!(
        serde_json::to_value(ProtocolError::task_check_unconfined()).expect("serialize"),
        json!({
            "class": "configuration",
            "code": "task_check_unconfined",
            "msg": "checks cannot be contained on this platform and unconfined checks are not allowed",
            "recover": "run without checks, or have the host owner set checks.allow_unconfined for the project",
        })
    );
    assert_eq!(
        serde_json::to_value(ProtocolError::task_snapshot_retired()).expect("serialize"),
        json!({
            "class": "runtime",
            "code": "task_snapshot_retired",
            "msg": "the turn's snapshots were retired with the session content",
        })
    );
}

#[test]
fn task_error_travels_in_a_response_envelope() {
    let response = Response::err(
        PROTOCOL_VERSION,
        "req-task-busy",
        ProtocolError::task_worktree_busy(),
    )
    .expect("valid response");
    let line = serde_json::to_string(&response).expect("serialize response");
    let back: Response = serde_json::from_str(&line).expect("deserialize response");
    assert_eq!(back, response);
    let value = serde_json::to_value(&response).expect("response as value");
    assert_eq!(value["err"]["class"], "runtime");
    assert_eq!(value["err"]["code"], "task_worktree_busy");
}
