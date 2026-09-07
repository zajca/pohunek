//! Integration coverage for durable stable-host bootstrap boundaries.

// Rust guideline compliant 2026-09-04

use std::io::{Read as _, Write as _};
use std::os::unix::{
    fs::{MetadataExt as _, PermissionsExt as _},
    net::{UnixListener, UnixStream},
};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use pohunek_daemon::host_state::{
    HostStateDir, HostStateError, HostStateRepository, HostStateRepositoryError,
};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt as _;

const CHILD_STATE_DIR_ENV: &str = "POHUNEK_TEST_HOST_STATE_DIR";
const CHILD_ATTEMPT_SOCKET_ENV: &str = "POHUNEK_TEST_HOST_ATTEMPT_SOCKET";
const CHILD_PERMIT_SOCKET_ENV: &str = "POHUNEK_TEST_HOST_PERMIT_SOCKET";
const CHILD_RESULT_SOCKET_ENV: &str = "POHUNEK_TEST_HOST_RESULT_SOCKET";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Bounds IPC without turning readiness into polling or an unbounded test hang.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounds child teardown if an assertion or IPC peer fails.
const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Prevents unbounded test-only event reads from a malformed child.
const MAX_IPC_EVENT_BYTES: u64 = 1024;

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChildEvent {
    LockContended,
    Bootstrap {
        host_id: String,
        approval_key_reference: String,
    },
    Error {
        stage: String,
    },
}

struct Children(Vec<Child>);

fn terminate_and_reap(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    let _ = child.wait_timeout(CHILD_WAIT_TIMEOUT);
}

impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            terminate_and_reap(child);
        }
    }
}

impl Children {
    fn wait_for_success(&mut self) {
        for child in &mut self.0 {
            let status = child
                .wait_timeout(CHILD_WAIT_TIMEOUT)
                .expect("wait for bootstrap child")
                .expect("bootstrap child must finish before the deadline");
            assert!(status.success(), "bootstrap child must succeed: {status}");
        }
    }
}

fn state_dir() -> tempfile::TempDir {
    let temp = tempfile::Builder::new()
        .prefix("pohunek-host-bootstrap-")
        .tempdir()
        .expect("create isolated state directory");
    std::fs::set_permissions(
        temp.path(),
        std::fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
    )
    .expect("make isolated state directory owner-private");
    temp
}

fn send_event(socket: std::ffi::OsString, event: &ChildEvent) {
    let bytes = serde_json::to_vec(event).expect("serialize child event");
    assert!(
        bytes.len() <= usize::try_from(MAX_IPC_EVENT_BYTES).expect("event bound fits usize"),
        "child event exceeds the bounded IPC frame"
    );
    let mut stream = UnixStream::connect(socket).expect("connect child event socket");
    stream
        .set_write_timeout(Some(IPC_TIMEOUT))
        .expect("set child event write deadline");
    stream.write_all(&bytes).expect("write child event");
}

fn accept_with_timeout(listener: &UnixListener, stage: &'static str) -> UnixStream {
    let listener = listener.try_clone().expect("clone IPC listener");
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(listener.accept().map(|(stream, _address)| stream));
    });
    receiver
        .recv_timeout(IPC_TIMEOUT)
        .unwrap_or_else(|_error| panic!("{stage} IPC accept timed out"))
        .unwrap_or_else(|_error| panic!("{stage} IPC accept failed"))
}

fn receive_event(listener: &UnixListener, stage: &'static str) -> ChildEvent {
    let stream = accept_with_timeout(listener, stage);
    stream
        .set_read_timeout(Some(IPC_TIMEOUT))
        .expect("set child event read deadline");
    let mut bytes = Vec::new();
    stream
        .take(MAX_IPC_EVENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .expect("read child event");
    assert!(
        bytes.len() <= usize::try_from(MAX_IPC_EVENT_BYTES).expect("event bound fits usize"),
        "child event exceeds the bounded IPC frame"
    );
    serde_json::from_slice(&bytes).expect("decode structured child event")
}

fn run_concurrent_bootstrap_child(path: std::ffi::OsString) {
    let attempt_socket = std::env::var_os(CHILD_ATTEMPT_SOCKET_ENV).expect("attempt socket");
    let permit_socket = std::env::var_os(CHILD_PERMIT_SOCKET_ENV).expect("permit socket");
    let result_socket = std::env::var_os(CHILD_RESULT_SOCKET_ENV).expect("result socket");
    let attempt = match HostStateDir::open_or_create(std::path::Path::new(&path))
        .and_then(|directory| directory.acquire_lock())
    {
        Err(HostStateError::LockContended { .. }) => ChildEvent::LockContended,
        Ok(_lock) => ChildEvent::Error {
            stage: "unexpected_lock_acquisition".to_owned(),
        },
        Err(_error) => ChildEvent::Error {
            stage: "lock_attempt".to_owned(),
        },
    };
    send_event(attempt_socket, &attempt);

    let mut permit = UnixStream::connect(permit_socket).expect("connect bootstrap permit socket");
    permit
        .set_read_timeout(Some(IPC_TIMEOUT))
        .expect("set bootstrap permit read deadline");
    let mut start = [0_u8; 1];
    permit
        .read_exact(&mut start)
        .expect("wait for bootstrap retry permit");

    let result = match HostStateRepository::open_or_create(path) {
        Ok(repository) => ChildEvent::Bootstrap {
            host_id: repository.snapshot().host_id().as_str().to_owned(),
            approval_key_reference: repository.approval_key_reference().as_str().to_owned(),
        },
        Err(_error) => ChildEvent::Error {
            stage: "bootstrap".to_owned(),
        },
    };
    send_event(result_socket, &result);
}

fn spawn_concurrent_bootstrap_children(
    temp: &tempfile::TempDir,
    attempt_socket: &std::path::Path,
    permit_socket: &std::path::Path,
    result_socket: &std::path::Path,
) -> Children {
    Children(
        (0..2)
            .map(|_child_index| {
                Command::new(std::env::current_exe().expect("test executable"))
                    .arg("--exact")
                    .arg("concurrent_first_start_waits_for_the_locked_bootstrap_then_converges")
                    .env(CHILD_STATE_DIR_ENV, temp.path())
                    .env(CHILD_ATTEMPT_SOCKET_ENV, attempt_socket)
                    .env(CHILD_PERMIT_SOCKET_ENV, permit_socket)
                    .env(CHILD_RESULT_SOCKET_ENV, result_socket)
                    .spawn()
                    .expect("spawn concurrent bootstrap child")
            })
            .collect(),
    )
}

fn assert_lock_contention(listener: &UnixListener) {
    for stage in ["first lock attempt", "second lock attempt"] {
        assert_eq!(receive_event(listener, stage), ChildEvent::LockContended);
    }
}

fn permit_bootstrap_retries(listener: &UnixListener) {
    let mut permits = [
        accept_with_timeout(listener, "first retry permit"),
        accept_with_timeout(listener, "second retry permit"),
    ];
    for permit in &mut permits {
        permit
            .set_write_timeout(Some(IPC_TIMEOUT))
            .expect("set retry permit write deadline");
        permit.write_all(&[1]).expect("permit bootstrap retry");
    }
}

fn assert_converged_results(listener: &UnixListener) -> (String, String) {
    let results = [
        receive_event(listener, "first bootstrap result"),
        receive_event(listener, "second bootstrap result"),
    ];
    let [ChildEvent::Bootstrap {
        host_id: first_host_id,
        approval_key_reference: first_reference,
    }, ChildEvent::Bootstrap {
        host_id: second_host_id,
        approval_key_reference: second_reference,
    }] = results
    else {
        panic!("children must return structured bootstrap successes")
    };
    assert_eq!(
        first_host_id, second_host_id,
        "children must converge on one host identity"
    );
    assert_eq!(
        first_reference, second_reference,
        "children must converge on one approval key"
    );
    (first_host_id, first_reference)
}

#[test]
fn bootstrap_records_are_owner_private_and_restart_stable() {
    let temp = state_dir();
    let first = HostStateRepository::open_or_create(temp.path()).expect("bootstrap repository");
    let first_snapshot = first.snapshot();
    let reference = first.approval_key_reference().clone();
    drop(first);

    let host = temp.path().join(pohunek_paths::HOST_STATE_SUBDIR);
    let identity: serde_json::Value = serde_json::from_slice(
        &std::fs::read(host.join(pohunek_paths::HOST_IDENTITY_NAME)).expect("read identity record"),
    )
    .expect("decode identity record");
    assert_eq!(identity["format_version"], 2);
    assert_eq!(identity["phase"], "complete");
    assert_eq!(
        std::fs::metadata(&host).expect("host metadata").mode() & 0o777,
        PRIVATE_DIRECTORY_MODE
    );
    for name in [
        pohunek_paths::HOST_IDENTITY_NAME,
        pohunek_paths::HOST_APPROVAL_KEY_NAME,
        pohunek_paths::HOST_GOVERNANCE_NAME,
        pohunek_paths::HOST_STATE_LOCK_NAME,
    ] {
        assert_eq!(
            std::fs::metadata(host.join(name))
                .expect("record metadata")
                .mode()
                & 0o777,
            PRIVATE_FILE_MODE
        );
    }

    let second = HostStateRepository::open_or_create(temp.path()).expect("restart repository");
    assert_eq!(second.snapshot(), first_snapshot);
    assert_eq!(second.approval_key_reference(), &reference);
}

#[test]
fn complete_identity_fails_closed_when_any_required_peer_record_is_lost() {
    for missing in [
        pohunek_paths::HOST_APPROVAL_KEY_NAME,
        pohunek_paths::HOST_GOVERNANCE_NAME,
    ] {
        let temp = state_dir();
        let repository =
            HostStateRepository::open_or_create(temp.path()).expect("bootstrap repository");
        drop(repository);
        let missing_path = temp
            .path()
            .join(pohunek_paths::HOST_STATE_SUBDIR)
            .join(missing);
        std::fs::remove_file(missing_path).expect("remove one completed peer record");
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::IncompleteRecordSet)
        ));
    }
}

#[test]
fn concurrent_first_start_waits_for_the_locked_bootstrap_then_converges() {
    if let Some(path) = std::env::var_os(CHILD_STATE_DIR_ENV) {
        run_concurrent_bootstrap_child(path);
        return;
    }

    let temp = state_dir();
    let directory = HostStateDir::open_or_create(temp.path()).expect("create empty host state");
    let lock = directory.acquire_lock().expect("hold external state lock");
    let attempt_socket = temp.path().join("bootstrap-attempt.sock");
    let permit_socket = temp.path().join("bootstrap-permit.sock");
    let result_socket = temp.path().join("bootstrap-result.sock");
    let attempt_listener = UnixListener::bind(&attempt_socket).expect("bind attempt listener");
    let permit_listener = UnixListener::bind(&permit_socket).expect("bind permit listener");
    let result_listener = UnixListener::bind(&result_socket).expect("bind result listener");
    let mut children =
        spawn_concurrent_bootstrap_children(&temp, &attempt_socket, &permit_socket, &result_socket);
    assert_lock_contention(&attempt_listener);
    drop(lock);
    permit_bootstrap_retries(&permit_listener);

    let (host_id, reference) = assert_converged_results(&result_listener);
    children.wait_for_success();
    let repository = HostStateRepository::open_or_create(temp.path())
        .expect("final reopen loads the completed state");
    assert_eq!(host_id, repository.snapshot().host_id().as_str());
    assert_eq!(reference, repository.approval_key_reference().as_str());
    drop(repository);
    let identity: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            temp.path()
                .join(pohunek_paths::HOST_STATE_SUBDIR)
                .join(pohunek_paths::HOST_IDENTITY_NAME),
        )
        .expect("read finalized identity"),
    )
    .expect("decode finalized identity");
    assert_eq!(identity["phase"], "complete");
}
