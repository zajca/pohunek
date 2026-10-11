// Rust guideline compliant 2026-10-11

use std::process::Command;

use pohunek_session_worker::JOURNAL_SCHEMA_VERSION;
use pohunek_test_support::bin_exe;
use pohunek_test_support::tempdir;
use pohunek_worker_protocol::SUPPORTED_RANGE;

#[test]
fn protocol_info_is_inert_machine_readable_and_uses_authoritative_versions() {
    let root = tempdir().expect("create temporary working directory");
    let output = Command::new(bin_exe("pohunek-sessiond"))
        .arg("--protocol-info")
        .env_clear()
        .current_dir(root.path())
        .output()
        .expect("run session worker capability command");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let info: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse capability response");
    assert_eq!(info["binary"], "pohunek-sessiond");
    assert_eq!(
        info["schema_version"],
        pohunek_worker_protocol::WORKER_DIAGNOSTIC_SCHEMA_VERSION
    );
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["journal_schema_version"], JOURNAL_SCHEMA_VERSION);
    assert_eq!(
        info["worker_protocol"]["minimum"],
        SUPPORTED_RANGE.minimum().get()
    );
    assert_eq!(
        info["worker_protocol"]["maximum"],
        SUPPORTED_RANGE.maximum().get()
    );
    assert_eq!(
        info.as_object().expect("object response").len(),
        5,
        "the response must not disclose host paths or environment"
    );
    assert_eq!(
        std::fs::read_dir(root.path())
            .expect("read temporary working directory")
            .count(),
        0,
        "the capability command must not create host state"
    );
}
