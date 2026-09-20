//! Real-process regression for durable state authority across runtime roots.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::net::UnixStream;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Host-state contention retries for five seconds before failing closed.
const CONTENTION_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

fn daemon_command(root: &Path, runtime: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .env_remove("POHUNEK_DAEMON_ID")
        .env_remove("POHUNEK_SESSION_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    command
}

fn worker_binary() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .expect("daemon crate is inside workspace")
                .join("target")
        },
        PathBuf::from,
    );
    target.join("debug/pohunek-sessiond")
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
    let root = tempfile::Builder::new()
        .prefix("pohunek-state-authority-")
        .tempdir_in("/tmp")
        .expect("create short test root");
    let runtime_one = root.path().join("r1");
    let runtime_two = root.path().join("r2");
    let socket_one = runtime_one.join("pohunek/daemon.sock");

    let mut first = daemon_command(root.path(), &runtime_one)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first daemon");
    wait_until_ready(&mut first, &socket_one).await;

    let second = daemon_command(root.path(), &runtime_two)
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
