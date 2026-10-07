//! Wire-contract tests for the delegated task layer errors (task RFC section
//! 13.1): stable codes, classes, recovery hints, payload-free text and the
//! exact JSON wire shape.

use std::collections::HashSet;

use protocol::{ErrorClass, ProtocolError};
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

/// Exact fixed `msg` and `recover` text of every task-layer error; the public
/// contract promises both as fixed text, so a reworded constructor must update
/// this table deliberately.
const TASK_ERROR_TEXTS: [(&str, &str, Option<&str>); 34] = [
    (
        "worktree_users_changed",
        "the expected worktree users differ from the current set",
        Some("re-read the worktree users with task.inspect and retry with the exact current set"),
    ),
    (
        "task_turn_open",
        "the task's latest turn is still open or already resumed",
        Some("wait for the turn to settle with task.wait, then retry"),
    ),
    (
        "task_attention_open",
        "the task's latest turn awaits an answer to its attention",
        Some("answer the pending attention with task.answer, or stop the task"),
    ),
    (
        "task_agent_busy",
        "the agent is still working on an earlier prompt",
        Some("wait with task.wait or extend the turn with task.extend, then retry; or stop the task"),
    ),
    (
        "task_worktree_busy",
        "the worktree is occupied by another task or live session",
        Some("inspect the worktree users with task.inspect and retry after the occupant settles or is stopped"),
    ),
    (
        "worktree_busy",
        "a task occupies this worktree; only observation is admitted",
        Some("observe the session read-only, or retry after the occupying task settles or is stopped"),
    ),
    (
        "task_worktree_unavailable",
        "the shared worktree no longer exists",
        Some("start a new task with its own worktree"),
    ),
    (
        "task_worktree_mode_conflict",
        "worktree_of cannot be combined with in_place or branch",
        Some("send worktree_of alone, or in_place or branch without worktree_of"),
    ),
    (
        "task_session_ended",
        "the session belongs to an ended task and cannot be revived outside the task layer",
        Some("continue the work with task.start and worktree_of naming the ended task"),
    ),
    (
        "task_session_unavailable",
        "the task has ended or its agent runtime is not live",
        Some("inspect the task with task.inspect; continue ended work with task.start and worktree_of"),
    ),
    (
        "task_turn_queued",
        "the turn is queued and has no deadline until it is delivered",
        Some("wait for delivery with task.wait before extending the turn"),
    ),
    (
        "task_worktree_via_investigate",
        "an executor task cannot join a worktree through an investigate-mode task",
        Some("name a task that is not in investigate mode in worktree_of, or start in investigate mode"),
    ),
    (
        "task_turn_ceiling_reached",
        "the turn reached its total open-time ceiling",
        Some("answer or stop the turn, or continue with a new turn once the agent is ready"),
    ),
    (
        "task_answer_unsupported",
        "this attention cannot be answered through the task layer",
        Some("resolve the attention by terminal takeover, or stop the task"),
    ),
    (
        "task_answer_unverifiable",
        "a keystroke answer cannot be verified against the provider's pending request",
        Some("set allow_unverified_delivery to accept an unverified answer, or resolve the attention in the terminal"),
    ),
    (
        "task_payload_mismatch",
        "the resubmitted payload does not match the stored request fingerprint; nothing was dispatched",
        Some("resend the exact original payload with the same request key, or inspect the task first"),
    ),
    (
        "task_stop_precondition_failed",
        "the stop preconditions do not hold; nothing was changed",
        Some("inspect the task and retry the stop with current preconditions"),
    ),
    (
        "task_attention_stale",
        "the named attention is not the current pending one; nothing was written",
        Some("inspect the task for the current attention and revision before answering again"),
    ),
    (
        "task_result_unknown",
        "no result with the requested result_id exists",
        Some("read the current result_id with task.result or task.inspect"),
    ),
    (
        "task_snapshot_retired",
        "the turn's snapshots were retired with the session content",
        None,
    ),
    (
        "task_cursor_expired",
        "the paging cursor expired or belongs to another daemon epoch",
        Some("restart the walk from the first page without a cursor"),
    ),
    (
        "worktree_in_use",
        "live sessions or active tasks use this worktree",
        Some("stop the sessions or tasks that use the worktree, then retry the removal"),
    ),
    (
        "task_result_pending",
        "the turn settled but its checks have not finished",
        Some("wait for publication with task.wait, then retry"),
    ),
    (
        "task_check_unconfined",
        "checks cannot be contained on this platform and unconfined checks are not allowed",
        Some("run without checks, or have the host owner set checks.allow_unconfined for the project"),
    ),
    (
        "task_check_not_permitted",
        "a requested check is not enabled or not permitted for this caller",
        Some("request only checks enabled for the project and permitted for this caller"),
    ),
    (
        "task_store_full",
        "a task store cap would be exceeded",
        Some("end finished tasks or wait for retention to free space; the host owner may raise the task store caps"),
    ),
    (
        "task_review_limit_reached",
        "the result already holds verdicts from the maximum number of reviewers",
        None,
    ),
    (
        "task_waiter_limit_reached",
        "the task waiter limit is currently reached",
        Some("retry the wait after another task wait completes"),
    ),
    (
        "task_fingerprint_key_missing",
        "the fingerprint key version of the stored request is unavailable; nothing was compared or dispatched",
        Some("inspect the task; the host owner must restore the task fingerprint key before resubmitting"),
    ),
    (
        "task_request_conflict",
        "the request key was already used with different parameters; nothing was executed",
        Some("use a new client_request_id for a different request, or resend the original parameters"),
    ),
    (
        "task_investigate_no_checks",
        "investigate-mode tasks accept neither checks nor checks_baseline",
        Some("omit checks and checks_baseline and rely on the executor task's checks"),
    ),
    (
        "task_fork_unsupported",
        "the task's agent cannot fork its session",
        Some("start a new task on the same worktree with worktree_of"),
    ),
    (
        "task_investigate_unsupported",
        "the selected profile cannot enforce investigation mode",
        Some("select a profile that can enforce investigation mode"),
    ),
    (
        "check_cleanup_stuck",
        "a daemon-owned check process in the worktree cannot be confirmed gone",
        Some("inspect the occupying task with task.inspect; the worktree stays occupied until its check processes are gone"),
    ),
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

/// The concatenated daemon sources, searched for hand-written code literals.
fn daemon_source_literals() -> String {
    fn walk(dir: &std::path::Path, out: &mut String) {
        for entry in std::fs::read_dir(dir).expect("read daemon source dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push_str(&std::fs::read_to_string(&path).expect("read daemon source"));
            }
        }
    }
    let root = pohunek_test_support::workspace_root().join("crates/daemon/src");
    let mut out = String::new();
    walk(&root, &mut out);
    out
}

#[test]
fn task_error_codes_are_never_hand_written_in_daemon_sources() {
    // Daemon handlers raise task errors through the canonical constructors; a
    // quoted task code in the daemon sources is a second, hand-written
    // definition that can drift from the contract or collide with it.
    let sources = daemon_source_literals();
    for case in task_errors() {
        let literal = format!("\"{}\"", case.code);
        assert!(
            !sources.contains(&literal),
            "{}: use the ProtocolError constructor instead of a literal",
            case.code
        );
    }
}

#[test]
fn shared_codes_are_documented_for_every_raising_method() {
    let docs =
        std::fs::read_to_string(pohunek_test_support::workspace_root().join("docs/public-api.md"))
            .expect("read public-api.md");
    let row = docs
        .lines()
        .find(|line| line.starts_with("| `worktree_in_use` |"))
        .expect("worktree_in_use table row");
    for method in ["`session.remove`", "`worktree.remove`"] {
        assert!(row.contains(method), "worktree_in_use row misses {method}");
    }
}

#[test]
fn task_error_texts_are_pinned_exactly() {
    let cases = task_errors();
    assert_eq!(cases.len(), TASK_ERROR_TEXTS.len());
    for (code, msg, recover) in TASK_ERROR_TEXTS {
        let case = cases
            .iter()
            .find(|case| case.code == code)
            .unwrap_or_else(|| panic!("{code}: missing from TASK_ERRORS"));
        assert_eq!(case.error.msg, msg, "{code}: msg");
        assert_eq!(case.error.recover.as_deref(), recover, "{code}: recover");
    }
}
