//! Integration coverage for owner-private host-state persistence.

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::{
    fs::{symlink, PermissionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use pohunek_daemon::host_state::{HostStateDir, HostStateError};

/// Bounds readiness, release, and child-process reaping for lock tests.
const LOCK_HELPER_TIMEOUT: Duration = Duration::from_secs(5);

fn temp_root(tag: &str) -> PathBuf {
    tempfile::Builder::new()
        .prefix(&format!("pohunek-host-state-{tag}-"))
        .tempdir()
        .expect("create isolated root")
        .keep()
}

fn state_dir(root: &Path) -> PathBuf {
    root.join("state").join("pohunek")
}

#[test]
fn creates_exactly_private_directories_and_records() {
    let root = temp_root("private");
    let host = HostStateDir::open_or_create(&state_dir(&root)).expect("open host state");
    host.replace_record("identity.json", b"stable-host-id")
        .expect("replace record");

    assert_eq!(
        host.read_record("identity.json").expect("read"),
        Some(b"stable-host-id".to_vec())
    );
    assert_eq!(
        fs::metadata(root.join("state"))
            .expect("state parent metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(state_dir(&root))
            .expect("application state metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(host.path())
            .expect("host metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(host.path().join("identity.json"))
            .expect("record metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn rejects_symlink_host_parent_and_record_without_touching_referents() {
    let root = temp_root("symlinks");
    let state = state_dir(&root);
    fs::create_dir_all(&state).expect("create state dir");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod state dir");
    let referent = root.join("referent");
    fs::create_dir_all(&referent).expect("create referent");
    symlink(&referent, state.join("host")).expect("host symlink");
    assert!(matches!(
        HostStateDir::open_or_create(&state),
        Err(HostStateError::Io { .. })
    ));
    assert!(referent.is_dir(), "host symlink referent remains untouched");

    let safe = HostStateDir::open_or_create(&state_dir(&temp_root("record-link")))
        .expect("open safe host");
    let target = root.join("record-target");
    fs::write(&target, b"keep").expect("write target");
    symlink(&target, safe.path().join("identity.json")).expect("record symlink");
    assert!(safe.replace_record("identity.json", b"new").is_err());
    assert_eq!(fs::read(&target).expect("read target"), b"keep");

    let outer = temp_root("parent-link");
    let actual = outer.join("actual");
    fs::create_dir_all(&actual).expect("create actual parent");
    symlink(&actual, outer.join("linked")).expect("parent symlink");
    HostStateDir::open_or_create(&outer.join("linked").join("pohunek"))
        .expect_err("parent symlink must be rejected");

    let state_parent = temp_root("state-parent-link");
    let state_referent = temp_root("state-parent-referent");
    symlink(&state_referent, state_parent.join("state")).expect("state parent symlink");
    HostStateDir::open_or_create(&state_dir(&state_parent))
        .expect_err("state parent symlink must be rejected");
    assert!(
        !state_referent.join("pohunek").exists(),
        "state-parent symlink referent remains untouched"
    );

    let app_parent = temp_root("application-parent-link");
    fs::create_dir(app_parent.join("state")).expect("create state parent");
    fs::set_permissions(app_parent.join("state"), fs::Permissions::from_mode(0o700))
        .expect("set state parent mode");
    let app_referent = temp_root("application-parent-referent");
    symlink(&app_referent, app_parent.join("state").join("pohunek"))
        .expect("application directory symlink");
    HostStateDir::open_or_create(&state_dir(&app_parent))
        .expect_err("application directory symlink must be rejected");
    assert!(
        !app_referent.join("host").exists(),
        "application-directory symlink referent remains untouched"
    );
}

#[test]
fn rejects_unsafe_modes_regular_substitution_and_hard_links() {
    let root = temp_root("unsafe");
    let state = state_dir(&root);
    fs::create_dir_all(&state).expect("create state");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).expect("loosen state mode");
    assert!(matches!(
        HostStateDir::open_or_create(&state),
        Err(HostStateError::UnsafePermissions { .. })
    ));

    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("tighten state mode");
    fs::write(state.join("host"), b"not a directory").expect("regular host substitute");
    HostStateDir::open_or_create(&state).expect_err("unsafe state directory mode must be rejected");

    fs::remove_file(state.join("host")).expect("remove substitute");
    let host = HostStateDir::open_or_create(&state).expect("open host");
    host.replace_record("governance.json", b"one")
        .expect("write record");
    fs::hard_link(
        host.path().join("governance.json"),
        root.join("linked-record"),
    )
    .expect("hard link");
    assert!(matches!(
        host.read_record("governance.json"),
        Err(HostStateError::UnsafePermissions { .. })
    ));
}

#[test]
fn lock_is_exclusive_and_releases_with_its_holder() {
    let root = temp_root("lock");
    let host = HostStateDir::open_or_create(&state_dir(&root)).expect("open host");
    let first = host.acquire_lock().expect("first lock");
    assert!(matches!(
        host.acquire_lock(),
        Err(HostStateError::LockContended { .. })
    ));
    drop(first);
    drop(host.acquire_lock().expect("lock reacquires after drop"));
}

#[test]
fn cross_process_lock_is_contended_and_releases_after_holder_exit() {
    let root = temp_root("cross-process-lock");
    let state = state_dir(&root);
    let holder = spawn_lock_holder(&root, &state);

    let host = HostStateDir::open_or_create(&state).expect("open shared host state");
    assert!(matches!(
        host.acquire_lock(),
        Err(HostStateError::LockContended { .. })
    ));
    holder.release_and_wait();
    drop(
        host.acquire_lock()
            .expect("lock releases after holder exits"),
    );
}

#[test]
fn replacing_a_held_lock_marker_cannot_create_a_second_process_writer() {
    let root = temp_root("cross-process-replaced-lock-marker");
    let state = state_dir(&root);
    let holder = spawn_lock_holder(&root, &state);
    let marker = state.join("host").join(pohunek_paths::HOST_STATE_LOCK_NAME);
    fs::remove_file(&marker).expect("unlink held lock marker");
    fs::write(&marker, b"replacement").expect("recreate lock marker");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600))
        .expect("restore replacement lock marker mode");

    let challenger = HostStateDir::open_or_create(&state).expect("open challenger host state");
    assert!(matches!(
        challenger.acquire_lock(),
        Err(HostStateError::LockContended { .. })
    ));

    holder.release_and_wait();
    drop(
        challenger
            .acquire_lock()
            .expect("replacement marker reacquires after directory lock releases"),
    );
}

struct LockHolder {
    child: Option<Child>,
    release: Option<UnixStream>,
}

impl LockHolder {
    fn release_and_wait(mut self) {
        self.release
            .take()
            .expect("lock-holder release connection is present")
            .write_all(&[1])
            .expect("release lock holder process");
        let mut child = self.child.take().expect("lock holder child is present");
        let status = wait_for_child(&mut child).expect("lock holder exits after release");
        assert!(status.success(), "lock holder exits successfully: {status}");
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        if let Some(release) = self.release.as_mut() {
            let _ = release.write_all(&[0]);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = wait_for_child(child);
        }
    }
}

fn spawn_lock_holder(root: &Path, state: &Path) -> LockHolder {
    let ready = root.join("holder-ready.sock");
    let release = root.join("holder-release.sock");
    let ready_listener = UnixListener::bind(&ready).expect("bind lock-holder readiness socket");
    let release_listener = UnixListener::bind(&release).expect("bind lock-holder release socket");
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "lock_holder_process", "--nocapture"])
        .env("POHUNEK_HOST_LOCK_HELPER_STATE", state)
        .env("POHUNEK_HOST_LOCK_HELPER_READY", &ready)
        .env("POHUNEK_HOST_LOCK_HELPER_RELEASE", &release)
        .spawn()
        .expect("spawn lock holder");
    let mut holder = LockHolder {
        child: Some(child),
        release: None,
    };
    let mut ready_stream = accept_with_timeout(&ready_listener, "lock-holder readiness");
    ready_stream
        .set_read_timeout(Some(LOCK_HELPER_TIMEOUT))
        .expect("bound lock-holder readiness signal");
    let mut signal = [0_u8; 1];
    ready_stream
        .read_exact(&mut signal)
        .expect("read bounded lock-holder readiness signal");
    assert_eq!(signal, [1], "lock holder publishes readiness");
    holder.release = Some(accept_with_timeout(
        &release_listener,
        "lock-holder release connection",
    ));
    holder
}

fn accept_with_timeout(listener: &UnixListener, operation: &str) -> UnixStream {
    listener
        .set_nonblocking(true)
        .expect("make lock-helper listener nonblocking");
    let deadline = Instant::now() + LOCK_HELPER_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _address)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "{operation} did not complete within {LOCK_HELPER_TIMEOUT:?}"
                );
                std::thread::yield_now();
            }
            Err(error) => panic!("{operation} failed: {error}"),
        }
    }
}

fn wait_for_child(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + LOCK_HELPER_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "lock holder did not exit before the cleanup deadline",
            ));
        }
        std::thread::yield_now();
    }
}

#[test]
fn lock_holder_process() {
    let Ok(state) = std::env::var("POHUNEK_HOST_LOCK_HELPER_STATE") else {
        return;
    };
    let ready = std::env::var("POHUNEK_HOST_LOCK_HELPER_READY").expect("helper ready path");
    let release = std::env::var("POHUNEK_HOST_LOCK_HELPER_RELEASE").expect("helper release path");
    let host = HostStateDir::open_or_create(Path::new(&state)).expect("open helper host state");
    let _lock = host.acquire_lock().expect("acquire helper lock");
    let mut release = UnixStream::connect(release).expect("connect lock-holder release socket");
    release
        .set_read_timeout(Some(LOCK_HELPER_TIMEOUT))
        .expect("bound lock-holder release wait");
    std::os::unix::net::UnixStream::connect(ready)
        .expect("connect lock-holder readiness socket")
        .write_all(&[1])
        .expect("publish helper readiness");
    let mut command = [0_u8; 1];
    release
        .read_exact(&mut command)
        .expect("receive bounded lock-holder release command");
}

#[test]
fn held_directory_descriptor_resists_parent_path_swap() {
    let root = temp_root("parent-swap");
    let state = state_dir(&root);
    let host = HostStateDir::open_or_create(&state).expect("open host");
    let moved = state.join("host-original");
    fs::rename(host.path(), &moved).expect("move held host directory");
    let attacker = root.join("attacker");
    fs::create_dir_all(&attacker).expect("create attacker directory");
    symlink(&attacker, state.join("host")).expect("replace path with symlink");

    host.replace_record("identity.json", b"safe")
        .expect("write through held descriptor");
    assert_eq!(
        fs::read(moved.join("identity.json")).expect("read original directory"),
        b"safe"
    );
    assert!(
        !attacker.join("identity.json").exists(),
        "path replacement must not receive record data"
    );
}
