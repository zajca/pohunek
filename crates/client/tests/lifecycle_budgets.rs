//! The TypeScript SDK derives its lifecycle response budgets from the same
//! step bound and sweep passes as this client; a shared fixture fails either
//! side when the two drift apart.

use std::path::PathBuf;

use pohunek_client::{
    LIFECYCLE_STEP_BOUND_MS, RESUME_SWEEP_PASSES, SESSION_FORK_REQUEST_BUDGET_MS,
    SESSION_NEW_REQUEST_BUDGET_MS, SESSION_RESUME_REQUEST_BUDGET_MS, SESSION_STOP_REQUEST_BUDGET_MS,
};
use serde_json::{json, Value};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/ts/sdk/test/fixtures/lifecycle-budgets.json")
}

#[test]
fn the_typescript_sdk_fixture_matches_the_client_budgets() {
    let expected = json!({
        "lifecycle_step_bound_ms": LIFECYCLE_STEP_BOUND_MS,
        "resume_sweep_passes": RESUME_SWEEP_PASSES,
        "session_new_request_budget_ms": SESSION_NEW_REQUEST_BUDGET_MS,
        "session_resume_request_budget_ms": SESSION_RESUME_REQUEST_BUDGET_MS,
        "session_fork_request_budget_ms": SESSION_FORK_REQUEST_BUDGET_MS,
        "session_stop_request_budget_ms": SESSION_STOP_REQUEST_BUDGET_MS,
        "session_remove_request_budget_ms": SESSION_STOP_REQUEST_BUDGET_MS,
    });
    let path = fixture_path();
    let stored = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; it must contain:\n{}",
            path.display(),
            serde_json::to_string_pretty(&expected).expect("serialize the expected fixture")
        )
    });
    let stored: Value = serde_json::from_str(&stored).expect("the fixture is JSON");
    assert_eq!(
        stored,
        expected,
        "update {} to:\n{}",
        path.display(),
        serde_json::to_string_pretty(&expected).expect("serialize the expected fixture")
    );
}
