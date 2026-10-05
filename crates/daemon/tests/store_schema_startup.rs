//! Real-process regression for the metadata store schema check at daemon startup.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pohunek_daemon::store::STORE_SCHEMA_VERSION;
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
        stderr.contains("schema version 99")
            && stderr.contains(&format!("schema version {STORE_SCHEMA_VERSION}")),
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
        assert_eq!(record["schema_version"], STORE_SCHEMA_VERSION, "{line}");
    }

    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;
}

/// The legacy-manifest file the importer reads at startup.
const LEGACY_MANIFEST: &str = "migrations/durable-session-workers.json";

/// The schema-1 store without its session record: the shape a legacy daemon
/// left behind, which is what `pohunek migration preflight` fingerprints.
fn resume_only_store() -> String {
    let mut store = String::new();
    for line in V0_33_0_STORE
        .lines()
        .filter(|line| !line.contains("\"kind\":\"session\""))
    {
        store.push_str(line);
        store.push('\n');
    }
    store
}

/// Writes a pending legacy manifest fingerprinting `store_content`.
fn write_pending_manifest(layout: &Layout, store_content: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let data_dir = layout.store.parent().expect("data dir");
    let path = data_dir.join(LEGACY_MANIFEST);
    fs::create_dir_all(path.parent().expect("migrations dir")).expect("create migrations dir");
    let manifest = serde_json::json!({
        "schema_version": 1,
        "created_at": "2026-07-01T00:00:00Z",
        "store_sha256": format!("{:x}", Sha256::digest(store_content.as_bytes())),
        "accept_runtime_loss": false,
        "live_session_ids": [],
        "sessions": [{
            "id": "s-legacy-done",
            "agent": "claude",
            "agent_base": "claude",
            "cwd": "/workspace/project",
            "pid": 1,
            "cols": 80,
            "rows": 24,
            "state": "done",
            "state_source": "process",
            "created_at": "2026-06-19T00:00:00Z",
            "updated_at": "2026-06-19T00:00:01Z"
        }],
    });
    fs::write(
        &path,
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write manifest");
    path
}

#[tokio::test]
async fn a_pending_legacy_manifest_still_imports_after_the_schema_migration() {
    let env = TestEnv::new().expect("create short test environment");
    let store_content = resume_only_store();
    let layout = Layout::new(&env, &store_content);
    let manifest = write_pending_manifest(&layout, &store_content);

    let mut daemon = daemon_command(&env, &layout)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    wait_until_ready(&mut daemon, &layout.socket()).await;

    assert!(!manifest.exists(), "the imported manifest is archived");
    let migrated = fs::read_to_string(&layout.store).expect("read store");
    assert!(
        migrated.contains("\"s-legacy-done\""),
        "the legacy session is imported: {migrated}"
    );
    assert_eq!(
        fs::read_to_string(layout.backup(1)).expect("read backup"),
        store_content,
        "the backup holds the bytes the manifest fingerprints"
    );

    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn a_store_edited_after_preflight_is_still_refused_after_the_schema_migration() {
    let env = TestEnv::new().expect("create short test environment");
    let preflight_content = resume_only_store();
    let edited = format!(
        "{preflight_content}{{\"kind\":\"project\",\"git_common_dir\":\"/other/.git\",\"repo_root\":\"/other\",\"is_bare\":false,\"source\":\"auto\",\"added_at\":\"t\",\"last_used_at\":\"t\"}}\n"
    );
    let layout = Layout::new(&env, &edited);
    let manifest = write_pending_manifest(&layout, &preflight_content);

    let output = daemon_command(&env, &layout)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon")
        .wait_with_output()
        .await
        .expect("collect daemon output");

    assert!(!output.status.success(), "startup must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("migration_store_changed"), "{stderr}");
    assert!(manifest.exists(), "the refused manifest stays pending");
    assert!(!layout.socket().exists(), "no listener before the refusal");
}
