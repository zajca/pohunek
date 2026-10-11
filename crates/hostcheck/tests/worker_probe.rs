use std::path::{Path, PathBuf};
use std::time::Duration;

use hostcheck::worker_probe::{probe_worker, WorkerProbeError};
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::tempdir;
use pohunek_worker_protocol::SUPPORTED_RANGE;

const EXPECTED_SCHEMA: u32 = 4;
/// Leaves ample startup time for process fixtures under loaded CI runners.
const PROBE_TEST_DEADLINE: Duration = Duration::from_secs(5);
/// Short bound used only to exercise the probe's timeout and cleanup path.
const TIMEOUT_TEST_DEADLINE: Duration = Duration::from_millis(150);

fn executable(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    write_executable(&path, format!("#!/bin/sh\n{body}\n")).expect("write probe executable");
    path
}

fn valid_info() -> &'static str {
    "printf '%s' '{\"schema_version\":1,\"binary\":\"pohunek-sessiond\",\"version\":\"1.2.3\",\"journal_schema_version\":4,\"worker_protocol\":{\"minimum\":5,\"maximum\":6}}'"
}

#[test]
fn accepts_a_valid_worker_response() {
    let root = tempdir().expect("create fixture root");
    let path = executable(root.path(), "worker", valid_info());

    let result = probe_worker(
        &path,
        PROBE_TEST_DEADLINE,
        &[EXPECTED_SCHEMA],
        SUPPORTED_RANGE,
    )
    .expect("accept compatible worker");

    assert_eq!(
        result.schema_version,
        pohunek_worker_protocol::WORKER_DIAGNOSTIC_SCHEMA_VERSION
    );
    assert_eq!(result.version, "1.2.3");
    assert_eq!(result.journal_schema_version, EXPECTED_SCHEMA);
    assert_eq!(result.protocol_minimum, 5);
    assert_eq!(result.protocol_maximum, 6);
}

#[test]
fn rejects_malformed_and_wrong_identity_responses() {
    let root = tempdir().expect("create fixture root");
    let malformed = executable(root.path(), "malformed", "printf '{not-json}'");
    let wrong_identity = executable(
        root.path(),
        "identity",
        "printf '%s' '{\"schema_version\":1,\"binary\":\"other\",\"version\":\"1\",\"journal_schema_version\":4,\"worker_protocol\":{\"minimum\":5,\"maximum\":6}}'",
    );

    assert!(matches!(
        probe_worker(
            &malformed,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::InvalidResponse)
    ));
    assert!(matches!(
        probe_worker(
            &wrong_identity,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::IdentityMismatch)
    ));
}

#[test]
fn recognizes_an_older_worker_without_the_capability_flag_as_unverified() {
    let root = tempdir().expect("create fixture root");
    let legacy = executable(
        root.path(),
        "legacy",
        "case \"$1\" in --protocol-info) printf '%s\\n' 'pohunek-sessiond: fatal: worker protocol failed: unknown worker argument `--protocol-info`' >&2; exit 2;; --version) printf '%s\\n' 'pohunek-sessiond 0.33.0';; esac",
    );

    assert!(matches!(
        probe_worker(
            &legacy,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::Unsupported)
    ));
}

// timing-allowed: #68 Exercises the deadline after a worker closes its pipes but stays alive.
#[test]
fn times_out_when_protocol_info_closes_output_but_the_process_hangs() {
    let root = tempdir().expect("create fixture root");
    let hangs_after_response = executable(
        root.path(),
        "hanging-worker",
        &format!(
            "case \"$1\" in --protocol-info) {}; exec 1>&- 2>&-; exec /bin/sleep 10;; --version) printf '%s\\n' 'pohunek-sessiond 0.33.0';; esac",
            valid_info()
        ),
    );

    assert!(matches!(
        probe_worker(
            &hangs_after_response,
            TIMEOUT_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::TimedOut)
    ));
}

#[test]
fn rejects_schema_or_protocol_mismatches() {
    let root = tempdir().expect("create fixture root");
    let path = executable(root.path(), "worker", valid_info());
    let only_new_protocol = executable(
        root.path(),
        "new-protocol",
        "printf '%s' '{\"schema_version\":1,\"binary\":\"pohunek-sessiond\",\"version\":\"1\",\"journal_schema_version\":4,\"worker_protocol\":{\"minimum\":7,\"maximum\":8}}'",
    );

    assert!(matches!(
        probe_worker(
            &path,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA + 1],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::Incompatible)
    ));
    assert!(matches!(
        probe_worker(
            &only_new_protocol,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::Incompatible)
    ));
}

#[test]
fn accepts_a_previously_readable_journal_schema_and_rejects_unknown_probe_schema() {
    let root = tempdir().expect("create fixture root");
    let previous_journal = executable(
        root.path(),
        "previous-journal",
        "printf '%s' '{\"schema_version\":1,\"binary\":\"pohunek-sessiond\",\"version\":\"1\",\"journal_schema_version\":3,\"worker_protocol\":{\"minimum\":5,\"maximum\":6}}'",
    );
    let unknown_probe_schema = executable(
        root.path(),
        "unknown-probe-schema",
        "printf '%s' '{\"schema_version\":2,\"binary\":\"pohunek-sessiond\",\"version\":\"1\",\"journal_schema_version\":4,\"worker_protocol\":{\"minimum\":5,\"maximum\":6}}'",
    );

    let result = probe_worker(
        &previous_journal,
        PROBE_TEST_DEADLINE,
        &[3, EXPECTED_SCHEMA],
        SUPPORTED_RANGE,
    )
    .expect("accept daemon-readable previous journal schema");
    assert_eq!(result.journal_schema_version, 3);
    assert!(matches!(
        probe_worker(
            &unknown_probe_schema,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::Incompatible)
    ));
}

#[test]
fn rejects_oversized_output_and_kills_the_child_group() {
    let root = tempdir().expect("create fixture root");
    let path = executable(root.path(), "large", "exec /usr/bin/yes x");

    assert!(matches!(
        probe_worker(
            &path,
            PROBE_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::OutputTooLarge)
    ));
}

// timing-allowed: #68 Exercises the wall-clock timeout and process-group cleanup.
#[test]
fn kills_a_timed_out_child_group() {
    let root = tempdir().expect("create fixture root");
    let path = executable(root.path(), "slow", "exec /bin/sleep 10");

    assert!(matches!(
        probe_worker(
            &path,
            TIMEOUT_TEST_DEADLINE,
            &[EXPECTED_SCHEMA],
            SUPPORTED_RANGE
        ),
        Err(WorkerProbeError::TimedOut)
    ));
}
