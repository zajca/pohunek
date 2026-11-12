//! Guards the class-1 list in `scripts/gate-receipt` against worker drift.
//!
//! The gate receipt tool strips the worker-injected session runtime names
//! from the wrapped gate's environment and never keys them, so receipts are
//! shared across sessions. This test runs on every session-worker PR (unlike
//! the Python counterpart in `scripts/tests/test_gate_receipt.py`, which CI
//! selects only for `scripts/` diffs), and fails when a worker change adds,
//! removes, or renames a `POHUNEK_*` name the child receives.

use std::collections::BTreeSet;
use std::path::PathBuf;

// Rust guideline compliant 2026-11-12

/// `POHUNEK_*` names the receipt tool strips and never keys. This must stay
/// identical to `SESSION_RUNTIME_VARS` in `scripts/gate-receipt` (the Python
/// guard there compares the same set against these sources from the other
/// side).
const EXPECTED_CLASS_ONE: &[&str] = &[
    "POHUNEK_BACKGROUND_LABEL_MAX_BYTES",
    "POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES",
    "POHUNEK_DAEMON_ID",
    "POHUNEK_ENV",
    "POHUNEK_HOOK_ADMISSION_RETRY",
    "POHUNEK_HOOK_RETRY_BACKOFF_MAX_MS",
    "POHUNEK_HOOK_RETRY_BACKOFF_MS",
    "POHUNEK_HOOK_RETRY_DEADLINE_MS",
    "POHUNEK_HOOK_TRACE_MAX_BYTES",
    "POHUNEK_HOOK_TRACE_PATH",
    "POHUNEK_NATIVE_REFERENCE_KIND",
    "POHUNEK_PROTOCOL_VERSION",
    "POHUNEK_SESSION_ID",
    "POHUNEK_SOCKET_PATH",
    "POHUNEK_WORKER_HOOK_PROTOCOL_VERSION",
    "POHUNEK_WORKER_ID",
    "POHUNEK_WORKER_INSTANCE_ID",
    "POHUNEK_WORKER_SOCKET_PATH",
];

/// Reads `SESSION_RUNTIME_VARS` from the receipt tool without executing it.
/// Only `UPPER_SNAKE = frozenset({...})` assignment lines are scanned, so no
/// tool code ever runs as part of this test.
fn receipt_class_one(tool: &std::path::Path) -> BTreeSet<String> {
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
            let name = line.trim().trim_matches(|trim: char| trim == '"' || trim == ',');
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

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .expect("crate two levels below the workspace root")
        .to_owned()
}

#[test]
fn receipt_class_one_matches_the_injected_session_environment() {
    let injected: BTreeSet<String> = super::SESSION_ENVIRONMENT_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let expected: BTreeSet<String> = EXPECTED_CLASS_ONE
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(
        injected, expected,
        "SESSION_ENVIRONMENT_NAMES drifted from the receipt tool's class-1 \
         list; update scripts/gate-receipt SESSION_RUNTIME_VARS in the same \
         change",
    );
    let tool = workspace_root().join("scripts").join("gate-receipt");
    assert_eq!(
        receipt_class_one(&tool),
        expected,
        "scripts/gate-receipt SESSION_RUNTIME_VARS drifted from the worker's \
         injected session environment; update it in the same change",
    );
}
