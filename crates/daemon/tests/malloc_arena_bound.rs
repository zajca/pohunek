//! Real-process check that the daemon bounds the glibc malloc arenas at startup
//! and leaves an operator-chosen limit alone.

// Rust guideline compliant 2026-10-31

#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;

use pohunek_daemon::malloc::{MALLOC_ARENA_MAX, OPERATOR_ARENA_VARIABLE};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{bin_exe, worker_binary};
use tokio::net::UnixStream;

/// Directory name the daemon uses below `XDG_DATA_HOME`.
const DATA_APP_DIR: &str = "pohunek";

/// Starts the daemon with `arena_variable` as the operator's
/// `MALLOC_ARENA_MAX` (or none), waits until it serves, stops it and returns
/// its stderr.
async fn startup_log(arena_variable: Option<&str>) -> String {
    let env = TestEnv::new().expect("create short test environment");
    let runtime = env.root().join("r");
    let data_home = env.root().join("data");
    let data_dir = data_home.join(DATA_APP_DIR);
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
        .expect("make data dir private");
    let socket: PathBuf = runtime.join("pohunek/daemon.sock");

    let mut command = env.tokio_command(bin_exe("pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", env.root().join("s"))
        .env("XDG_DATA_HOME", &data_home)
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .env_remove("GLIBC_TUNABLES")
        .env_remove(OPERATOR_ARENA_VARIABLE)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(value) = arena_variable {
        command.env(OPERATOR_ARENA_VARIABLE, value);
    }
    let mut daemon = command.spawn().expect("spawn daemon");
    let connected = wait_until("the daemon control socket accepts connections", || async {
        UnixStream::connect(&socket).await.ok()
    });
    tokio::select! {
        _stream = connected => {}
        status = daemon.wait() => panic!("daemon exited before readiness: {status:?}"),
    }
    daemon.kill().await.expect("stop daemon");
    let output = daemon
        .wait_with_output()
        .await
        .expect("collect daemon output");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn the_daemon_bounds_its_malloc_arenas_and_logs_the_limit() {
    let log = startup_log(None).await;

    assert!(
        log.contains("malloc arenas bounded")
            && log.contains(&format!("\"malloc_arena_max\":{MALLOC_ARENA_MAX}")),
        "the startup log names the applied limit: {log}"
    );
}

#[tokio::test]
async fn an_operator_arena_limit_is_left_alone() {
    let log = startup_log(Some("8")).await;

    assert!(
        log.contains("malloc arena limit set by the operator environment"),
        "the startup log says the operator's limit stands: {log}"
    );
    assert!(
        !log.contains("malloc arenas bounded"),
        "the daemon does not apply its own limit: {log}"
    );
}

#[tokio::test]
async fn an_arena_limit_glibc_ignores_does_not_stop_the_bound() {
    let log = startup_log(Some("")).await;

    assert!(
        log.contains("malloc arenas bounded"),
        "an empty MALLOC_ARENA_MAX is not an operator limit: {log}"
    );
}
