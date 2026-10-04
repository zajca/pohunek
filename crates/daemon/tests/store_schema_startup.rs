//! Real-process regression for the metadata store schema check at daemon startup.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{bin_exe, worker_binary};
use tokio::net::UnixStream;

/// Store written by the v0.33.0 daemon (schema 1).
const V0_33_0_STORE: &str = include_str!("../src/store/fixtures/v0.33.0/metadata.jsonl");

/// Directory name the daemon uses below `XDG_DATA_HOME`.
const DATA_APP_DIR: &str = "pohunek";

struct Layout {
    runtime: PathBuf,
    state_home: PathBuf,
    data_home: PathBuf,
    store: PathBuf,
}

impl Layout {
    fn new(env: &TestEnv, store_content: &str) -> Self {
        let data_home = env.root().join("data");
        let data_dir = data_home.join(DATA_APP_DIR);
        fs::create_dir_all(&data_dir).expect("create data dir");
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700))
            .expect("make data dir private");
        let store = data_dir.join("metadata.jsonl");
        fs::write(&store, store_content).expect("write store");
        fs::set_permissions(&store, fs::Permissions::from_mode(0o600)).expect("make store private");
        Self {
            runtime: env.root().join("r"),
            state_home: env.root().join("s"),
            data_home,
            store,
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("pohunek/daemon.sock")
    }

    fn backup(&self, schema: u32) -> PathBuf {
        self.store
            .with_file_name(format!("metadata.jsonl.pre-schema-{schema}"))
    }
}

fn daemon_command(env: &TestEnv, layout: &Layout) -> tokio::process::Command {
    let mut command = env.tokio_command(bin_exe("pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", &layout.runtime)
        .env("XDG_STATE_HOME", &layout.state_home)
        .env("XDG_DATA_HOME", &layout.data_home)
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn wait_until_ready(child: &mut tokio::process::Child, socket: &Path) {
    let connected = wait_until("the daemon control socket accepts connections", || async {
        UnixStream::connect(socket).await.ok()
    });
    tokio::select! {
        _stream = connected => {}
        status = child.wait() => panic!("daemon exited before readiness: {status:?}"),
    }
}

#[tokio::test]
async fn a_store_newer_than_the_daemon_fails_startup_and_is_left_untouched() {
    let env = TestEnv::new().expect("create short test environment");
    let newer = "{\"kind\":\"project\",\"schema_version\":99,\"future\":true}\n";
    let layout = Layout::new(&env, newer);

    let output = daemon_command(&env, &layout)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon")
        .wait_with_output()
        .await
        .expect("collect daemon output");

    assert!(!output.status.success(), "startup must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("schema version 99") && stderr.contains("schema version 2"),
        "the error names both versions: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&layout.store).expect("read store"),
        newer
    );
    assert!(!layout.backup(99).exists());
    assert!(!layout.socket().exists(), "no listener before the refusal");
}

#[tokio::test]
async fn an_older_store_is_migrated_with_a_backup_before_the_daemon_serves() {
    let env = TestEnv::new().expect("create short test environment");
    let layout = Layout::new(&env, V0_33_0_STORE);

    let mut daemon = daemon_command(&env, &layout)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    wait_until_ready(&mut daemon, &layout.socket()).await;

    assert_eq!(
        fs::read_to_string(layout.backup(1)).expect("read backup"),
        V0_33_0_STORE,
        "the backup holds the pre-migration store"
    );
    let migrated = fs::read_to_string(&layout.store).expect("read store");
    assert_eq!(migrated.lines().count(), 4);
    for line in migrated.lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("store line is json");
        assert_eq!(record["schema_version"], 2, "{line}");
    }

    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;
}
