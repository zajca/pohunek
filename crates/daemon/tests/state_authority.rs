//! Real-process regression for durable state authority across runtime roots.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::{bin_exe, worker_binary};
use tokio::net::UnixStream;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Host-state contention retries for five seconds before failing closed.
const CONTENTION_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Builds the daemon command for `env`: a scrubbed environment whose XDG
/// runtime, state and data bases are the given directories. HOME, the cache and
/// the config base are the private ones of `env`.
fn daemon_command(
    env: &TestEnv,
    runtime: &Path,
    state_home: &Path,
    data_home: &Path,
) -> tokio::process::Command {
    let mut command = env.tokio_command(bin_exe("pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state_home)
        .env("XDG_DATA_HOME", data_home)
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn wait_until_ready(child: &mut tokio::process::Child, socket: &Path) {
    tokio::time::timeout(STARTUP_TIMEOUT, async {
        loop {
            assert!(
                child.try_wait().expect("inspect daemon").is_none(),
                "daemon exited before readiness"
            );
            if UnixStream::connect(socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    })
    .await
    .expect("daemon becomes ready");
}

#[tokio::test]
async fn shared_state_rejects_second_daemon_with_a_different_runtime_root() {
    let env = TestEnv::new().expect("create short test environment");
    let runtime_one = env.root().join("r1");
    let runtime_two = env.root().join("r2");
    let state_home = env.state_home().to_path_buf();
    let data_one = env.root().join("d1");
    let data_two = env.root().join("d2");
    let socket_one = runtime_one.join("pohunek/daemon.sock");

    let mut first = daemon_command(&env, &runtime_one, &state_home, &data_one)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first daemon");
    wait_until_ready(&mut first, &socket_one).await;

    let second = daemon_command(&env, &runtime_two, &state_home, &data_two)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn competing daemon");
    let output = tokio::time::timeout(CONTENTION_TIMEOUT, second.wait_with_output())
        .await
        .expect("competing daemon exits promptly")
        .expect("collect competing daemon output");
    assert!(
        !output.status.success(),
        "competing daemon must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("host governance startup failed"),
        "failure identifies durable state authority rejection: {stderr}"
    );
    assert!(
        !runtime_two.join("pohunek/daemon.sock").exists(),
        "losing daemon must fail before mutating its runtime root"
    );
    assert!(
        UnixStream::connect(&socket_one).await.is_ok(),
        "the authoritative daemon remains healthy"
    );

    first.kill().await.expect("stop first daemon");
    let _ = first.wait().await;
}

#[tokio::test]
async fn shared_data_rejects_second_daemon_with_a_different_state_root() {
    let env = TestEnv::new().expect("create short test environment");
    let runtime_one = env.root().join("r1");
    let runtime_two = env.root().join("r2");
    let state_one = env.root().join("s1");
    let state_two = env.root().join("s2");
    let data_home = env.root().join("data");
    let socket_one = runtime_one.join("pohunek/daemon.sock");

    let mut first = daemon_command(&env, &runtime_one, &state_one, &data_home)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first daemon");
    wait_until_ready(&mut first, &socket_one).await;

    let second = daemon_command(&env, &runtime_two, &state_two, &data_home)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn competing daemon");
    let output = tokio::time::timeout(CONTENTION_TIMEOUT, second.wait_with_output())
        .await
        .expect("competing daemon exits promptly")
        .expect("collect competing daemon output");
    assert!(
        !output.status.success(),
        "competing daemon must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("another pohunek daemon is already running"),
        "failure identifies durable data authority rejection: {stderr}"
    );
    assert!(
        !state_two.join("pohunek").exists(),
        "losing daemon must fail before mutating its independent state root"
    );
    assert!(
        !runtime_two.join("pohunek/daemon.sock").exists(),
        "losing daemon must fail before mutating its runtime root"
    );
    assert!(
        UnixStream::connect(&socket_one).await.is_ok(),
        "the authoritative daemon remains healthy"
    );

    first.kill().await.expect("stop first daemon");
    let _ = first.wait().await;
}

#[tokio::test]
async fn identical_runtime_and_data_base_starts_without_self_deadlock() {
    let env = TestEnv::new().expect("create short test environment");
    let shared_home = env.root().join("shared");
    let state_home = env.state_home().to_path_buf();
    let socket = shared_home.join("pohunek/daemon.sock");

    let mut daemon = daemon_command(&env, &shared_home, &state_home, &shared_home)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon with a shared runtime and data base");
    wait_until_ready(&mut daemon, &socket).await;
    assert!(
        UnixStream::connect(&socket).await.is_ok(),
        "daemon with a shared runtime and data base must remain healthy"
    );

    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;
}
