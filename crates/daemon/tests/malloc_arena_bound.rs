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

/// Environment variable of glibc tunables.
const GLIBC_TUNABLES: &str = "GLIBC_TUNABLES";

/// Starts the daemon with `operator_env` added to its environment, waits until
/// it serves, stops it and returns its stderr.
async fn startup_log(operator_env: &[(&str, &str)]) -> String {
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
        .env_remove(GLIBC_TUNABLES)
        .env_remove(OPERATOR_ARENA_VARIABLE)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, value) in operator_env {
        command.env(name, value);
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
async fn the_daemon_bounds_its_malloc_arenas_before_any_other_thread_exists() {
    let log = startup_log(&[]).await;

    assert!(
        log.contains("malloc arenas bounded")
            && log.contains(&format!("\"malloc_arena_max\":{MALLOC_ARENA_MAX}")),
        "the startup log names the applied limit: {log}"
    );
    assert!(
        log.contains("\"threads_at_bound\":1"),
        "the bound is applied while only the main thread exists: {log}"
    );
}

/// The daemon neither applies its own limit nor claims one.
fn assert_left_alone(log: &str) {
    assert!(
        log.contains("malloc arena limit named by the operator environment"),
        "the startup log says the operator named the limit: {log}"
    );
    assert!(
        !log.contains("malloc arenas bounded"),
        "the daemon does not apply its own limit: {log}"
    );
}

#[tokio::test]
async fn an_operator_arena_limit_is_left_alone() {
    assert_left_alone(&startup_log(&[(OPERATOR_ARENA_VARIABLE, "8")]).await);
}

/// glibc reads `0x8` as 8; the presence of the variable is the operator's
/// intent whatever the notation.
#[tokio::test]
async fn an_operator_arena_limit_in_another_notation_is_left_alone() {
    assert_left_alone(&startup_log(&[(OPERATOR_ARENA_VARIABLE, "0x8")]).await);
}

#[tokio::test]
async fn an_operator_arena_tunable_is_left_alone() {
    assert_left_alone(&startup_log(&[(GLIBC_TUNABLES, "glibc.malloc.arena_max=8")]).await);
}

/// Other tunables are not an arena choice.
#[tokio::test]
async fn an_unrelated_tunable_does_not_stop_the_bound() {
    let log = startup_log(&[(GLIBC_TUNABLES, "glibc.malloc.tcache_count=4")]).await;

    assert!(
        log.contains("malloc arenas bounded"),
        "the daemon applies its limit: {log}"
    );
}
