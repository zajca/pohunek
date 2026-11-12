//! Guards the class-1 list in `scripts/gate-receipt` against worker drift.
//!
//! The gate receipt tool strips the worker-injected session runtime names
//! from the wrapped gate's environment and never keys them, so receipts are
//! shared across sessions. `command_from_initialize` injects exactly
//! `SESSION_ENVIRONMENT_NAMES` (plus `HOOK_RETRY_ENV_NAMES`, which the
//! profile filter passes through). This test compares the tool's list with
//! the union of both sources, so a change on either side — a new name in
//! the worker, a new retry name in worker-protocol, or an edit to the
//! tool — fails on the PR that causes it. The Python counterpart in
//! `scripts/tests/test_gate_receipt.py` guards the same list from the
//! `scripts/` side.

// Rust guideline compliant 2026-11-12

use std::collections::BTreeSet;
use std::path::Path;

use super::SESSION_ENVIRONMENT_NAMES;

/// Reads `SESSION_RUNTIME_VARS` from the receipt tool without executing it.
/// Only `UPPER_SNAKE = frozenset({...})` assignment lines are scanned, so no
/// tool code ever runs as part of this test.
fn receipt_class_one(tool: &Path) -> BTreeSet<String> {
    let source = std::fs::read_to_string(tool).expect("read scripts/gate-receipt");
    let mut names = BTreeSet::new();
    let mut in_set = false;
    for line in source.lines() {
        if line.starts_with("SESSION_RUNTIME_VARS = frozenset({") {
            in_set = true;
            continue;
        }
        if in_set {
            if line.starts_with("})") {
                break;
            }
            let name = line
                .trim()
                .trim_matches(|trim: char| trim == '"' || trim == ',');
            if name.starts_with("POHUNEK_") {
                names.insert(name.to_owned());
            }
        }
    }
    assert!(
        !names.is_empty(),
        "SESSION_RUNTIME_VARS not found in {}",
        tool.display()
    );
    names
}

#[test]
fn receipt_class_one_matches_the_injected_session_environment() {
    let injected: BTreeSet<String> = SESSION_ENVIRONMENT_NAMES
        .iter()
        .chain(pohunek_worker_protocol::HOOK_RETRY_ENV_NAMES.iter())
        .map(|name| (*name).to_owned())
        .collect();
    let tool = pohunek_test_support::workspace_root()
        .join("scripts")
        .join("gate-receipt");
    assert_eq!(
        receipt_class_one(&tool),
        injected,
        "scripts/gate-receipt SESSION_RUNTIME_VARS drifted from the union of \
         SESSION_ENVIRONMENT_NAMES and HOOK_RETRY_ENV_NAMES; update the tool \
         in the same change",
    );
}
