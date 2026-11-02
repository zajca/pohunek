//! A notification store this build cannot use safely leaves the daemon running.
//!
//! Each scenario starts the real daemon over a notification directory written
//! by a newer build or damaged on one line, and drives it over its control
//! socket: the notification RPCs answer a typed error, a session RPC still
//! works, and no notification file changes. A healthy start writes the format
//! marker.

// Rust guideline compliant 2026-10-30

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pohunek_client::ClientError;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::{
    method, NotificationCreateParams, NotificationKind, NotificationListParams,
    NotificationSeverity, NotificationSource, SessionNewParams,
};
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;

const DATA_APP_DIR: &str = "pohunek";

/// A policy the daemon accepts.
const VALID_POLICY: &str = r#"{"attention_dedupe_window_secs":300,"enabled":{"agent_blocked":true,"approval_required":true,"turn_completed":true,"session_finished":true,"error":true,"system":true},"retention":{"sweep_interval_secs":60,"info_ttl_secs":1,"warning_ttl_secs":1,"resolved_attention_ttl_secs":1,"resolved_error_ttl_secs":1,"archived_ttl_secs":1,"compaction_min_actions":1}}"#;

const NOTIFICATION_FILES: [&str; 3] = ["notifications.jsonl", "policy.json", "format.json"];

struct Layout {
    runtime: PathBuf,
    state_home: PathBuf,
    data_home: PathBuf,
    notifications: PathBuf,
}

impl Layout {
    fn new(env: &TestEnv, files: &[(&str, &[u8])]) -> Self {
        let data_home = env.root().join("data");
        let notifications = data_home.join(DATA_APP_DIR).join("notifications");
        std::fs::create_dir_all(&notifications).expect("create the notification directory");
        for directory in [data_home.join(DATA_APP_DIR), notifications.clone()] {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("make the directory private");
        }
        for (name, content) in files {
            let path = notifications.join(name);
            std::fs::write(&path, content).expect("write a notification file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("make the file private");
        }
        Self {
            runtime: env.root().join("r"),
            state_home: env.root().join("s"),
            data_home,
            notifications,
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("pohunek/daemon.sock")
    }

    /// The content of every notification file, `None` for an absent one.
    fn snapshot(&self) -> BTreeMap<&'static str, Option<Vec<u8>>> {
        NOTIFICATION_FILES
            .iter()
            .map(|name| (*name, std::fs::read(self.notifications.join(name)).ok()))
            .collect()
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
        .stderr(Stdio::piped())
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

fn create_params() -> NotificationCreateParams {
    NotificationCreateParams {
        source: NotificationSource {
            provider: "codex".to_owned(),
            provider_event: "PermissionRequest".to_owned(),
            host_local_source_id: "codex-hook-1".to_owned(),
        },
        kind: NotificationKind::ApprovalRequired,
        severity: NotificationSeverity::ActionRequired,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for owner approval.".to_owned(),
        metadata: BTreeMap::new(),
        session_id: None,
        agent_kind: None,
        source_id: None,
        dedupe_key: None,
        project_id: None,
        retryable_admission: false,
    }
}

fn shell_session(cwd: &Path) -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "shell".to_owned(),
        cwd: Some(cwd.to_path_buf()),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: BTreeMap::new(),
        secret_env: protocol::SessionSecretEnv::default(),
    }
}

fn unavailable_code(error: &ClientError) -> &str {
    match error {
        ClientError::Protocol(source) => source.code.as_str(),
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

/// A named set of files (name, bytes) that make the store unavailable.
type UnavailableCase = (&'static str, Vec<(&'static str, &'static [u8])>);

/// A store written in a newer format, a damaged format marker, an action this
/// build cannot decode and a policy key it rejects.
fn unavailable_cases() -> Vec<UnavailableCase> {
    vec![
        (
            "newer format",
            vec![("format.json", b"{\"format_version\":99}\n")],
        ),
        (
            "unreadable format marker",
            vec![("format.json", b"{not json\n")],
        ),
        (
            "undecodable action",
            vec![(
                "notifications.jsonl",
                b"{\"action\":\"created\",\"record\":{\"kind\":\"a_kind_from_a_newer_daemon\"}}\n{\"action\":\"created_with_something_new\"}\n",
            )],
        ),
        (
            "log that is not text",
            vec![("notifications.jsonl", b"{\"action\":\"created\"}\n\xff\xfe\n")],
        ),
        (
            "marker that is not text",
            vec![("format.json", b"\xff\xfe")],
        ),
        (
            "policy that parses but is invalid",
            vec![(
                "policy.json",
                b"{\"attention_dedupe_window_secs\":300,\"enabled\":{\"agent_blocked\":true,\"approval_required\":true,\"turn_completed\":true,\"session_finished\":true,\"error\":true,\"system\":true},\"retention\":{\"sweep_interval_secs\":0,\"info_ttl_secs\":1,\"warning_ttl_secs\":1,\"resolved_attention_ttl_secs\":1,\"resolved_error_ttl_secs\":1,\"archived_ttl_secs\":1,\"compaction_min_actions\":1}}\n",
            )],
        ),
        (
            "rejected policy key",
            vec![("policy.json", b"{\"a_key_from_a_newer_daemon\":1}\n")],
        ),
        (
            "valid policy with a key this build does not know",
            vec![(
                "policy.json",
                b"{\"attention_dedupe_window_secs\":300,\"enabled\":{\"agent_blocked\":true,\"approval_required\":true,\"turn_completed\":true,\"session_finished\":true,\"error\":true,\"system\":true},\"a_key_from_a_newer_daemon\":1}\n",
            )],
        ),
        (
            "valid policy with a retention key this build does not know",
            vec![(
                "policy.json",
                b"{\"attention_dedupe_window_secs\":300,\"enabled\":{\"agent_blocked\":true,\"approval_required\":true,\"turn_completed\":true,\"session_finished\":true,\"error\":true,\"system\":true},\"retention\":{\"sweep_interval_secs\":60,\"info_ttl_secs\":1,\"warning_ttl_secs\":1,\"resolved_attention_ttl_secs\":1,\"resolved_error_ttl_secs\":1,\"archived_ttl_secs\":1,\"compaction_min_actions\":1,\"a_key_from_a_newer_daemon\":1}}\n",
            )],
        ),
        (
            "valid policy with a kind this build does not know",
            vec![(
                "policy.json",
                b"{\"attention_dedupe_window_secs\":300,\"enabled\":{\"agent_blocked\":true,\"a_kind_from_a_newer_daemon\":true}}\n",
            )],
        ),
        (
            "valid policy with a provider override of a kind this build does not know",
            vec![(
                "policy.json",
                b"{\"attention_dedupe_window_secs\":300,\"enabled\":{\"agent_blocked\":true},\"providers\":{\"codex\":{\"a_kind_from_a_newer_daemon\":false}}}\n",
            )],
        ),
    ]
}

/// Every notification RPC answers `notifications_unavailable`.
async fn assert_notification_calls_refused(client: &mut pohunek_client::Client, case: &str) {
    let listed = client
        .call::<method::NotificationList>(NotificationListParams::default())
        .await
        .expect_err("listing needs the store");
    assert_eq!(
        unavailable_code(&listed),
        "notifications_unavailable",
        "{case}"
    );
    let created = client
        .call::<method::NotificationCreate>(create_params())
        .await
        .expect_err("creating needs the store");
    assert_eq!(
        unavailable_code(&created),
        "notifications_unavailable",
        "{case}"
    );
    let mut debounced = create_params();
    "scenario-tool".clone_into(&mut debounced.source.provider);
    debounced.session_id = Some(protocol::SessionId("s-1".to_owned()));
    debounced.dedupe_key = Some("attention:s-1".to_owned());
    let created = client
        .call::<method::NotificationCreate>(debounced)
        .await
        .expect_err("a debounced create is refused before it is minted");
    assert_eq!(
        unavailable_code(&created),
        "notifications_unavailable",
        "{case}"
    );
    let policy = client
        .call::<method::NotificationPolicyGet>(())
        .await
        .expect_err("the policy is not served from defaults");
    assert_eq!(
        unavailable_code(&policy),
        "notifications_unavailable",
        "{case}"
    );
    let updated = client
        .call::<method::NotificationUpdate>(protocol::NotificationUpdateParams {
            id: protocol::NotificationId("n-1".to_owned()),
            status: protocol::NotificationStatus::Read,
        })
        .await
        .expect_err("updating needs the store");
    assert_eq!(
        unavailable_code(&updated),
        "notifications_unavailable",
        "{case}"
    );
    let deleted = client
        .call::<method::NotificationDelete>(protocol::NotificationDeleteParams {
            id: protocol::NotificationId("n-1".to_owned()),
        })
        .await
        .expect_err("deleting needs the store");
    assert_eq!(
        unavailable_code(&deleted),
        "notifications_unavailable",
        "{case}"
    );
    let policy_set = client
        .call::<method::NotificationPolicySet>(protocol::NotificationPolicyParams {
            policy: serde_json::from_str(VALID_POLICY).expect("a valid policy"),
        })
        .await
        .expect_err("writing the policy needs the store");
    assert_eq!(
        unavailable_code(&policy_set),
        "notifications_unavailable",
        "{case}"
    );
    let pruned =
        client
            .call::<method::NotificationRetentionPrune>(
                protocol::NotificationRetentionParams::default(),
            )
            .await
            .expect_err("pruning needs the store");
    assert_eq!(
        unavailable_code(&pruned),
        "notifications_unavailable",
        "{case}"
    );
}

#[tokio::test]
async fn a_store_this_build_cannot_use_leaves_the_daemon_running_and_the_files_untouched() {
    for (case, files) in unavailable_cases() {
        let env = TestEnv::new().expect("create short test environment");
        let layout = Layout::new(&env, &files);
        let before = layout.snapshot();

        let mut daemon = daemon_command(&env, &layout).spawn().expect("spawn daemon");
        wait_until_ready(&mut daemon, &layout.socket()).await;
        let mut client = pohunek_client::Client::connect_local(&layout.socket())
            .await
            .expect("connect to the daemon");

        assert_notification_calls_refused(&mut client, case).await;

        let session = client
            .session_new(shell_session(env.root()))
            .await
            .unwrap_or_else(|error| panic!("{case}: a session RPC still works: {error:?}"));
        client
            .session_remove(session.session.id)
            .await
            .unwrap_or_else(|error| panic!("{case}: the session is removed: {error:?}"));
        drop(client);

        daemon.kill().await.expect("stop daemon");
        let mut stderr = String::new();
        daemon
            .stderr
            .take()
            .expect("piped stderr")
            .read_to_string(&mut stderr)
            .await
            .expect("read daemon stderr");
        let _ = daemon.wait().await;
        assert_eq!(
            layout.snapshot(),
            before,
            "{case}: no notification file changed"
        );
        assert!(
            stderr.contains("notifications are unavailable"),
            "{case}: the daemon surfaces the reason and the remedy: {stderr}"
        );
    }
}

#[tokio::test]
async fn a_healthy_store_writes_the_format_marker_and_stays_available() {
    let env = TestEnv::new().expect("create short test environment");
    let layout = Layout::new(&env, &[]);

    let mut daemon = daemon_command(&env, &layout).spawn().expect("spawn daemon");
    wait_until_ready(&mut daemon, &layout.socket()).await;
    let mut client = pohunek_client::Client::connect_local(&layout.socket())
        .await
        .expect("connect to the daemon");
    client
        .call::<method::NotificationCreate>(create_params())
        .await
        .expect("a healthy store accepts a notification");
    let listed = client
        .call::<method::NotificationList>(NotificationListParams::default())
        .await
        .expect("a healthy store lists");
    assert_eq!(listed.notifications.len(), 1);
    drop(client);
    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;

    let marker = std::fs::read_to_string(layout.notifications.join("format.json"))
        .expect("the format marker is written");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&marker).expect("marker json"),
        serde_json::json!({"format_version": 2})
    );
}

/// A store a format-1 build wrote opens as it is: its marker is rewritten to
/// the current format, and a record without the `hook_admitted` key still
/// ranks by its provider name, so a projector report inside the dedupe window
/// does not displace it.
#[tokio::test]
async fn a_format_1_store_opens_keeps_its_ranking_and_moves_to_format_2() {
    let env = TestEnv::new().expect("create short test environment");
    let created_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format the timestamp");
    let legacy = serde_json::json!({
        "action": "created",
        "record": {
            "id": "n-legacy",
            "source": {
                "provider": "hermes",
                "provider_event": "PermissionRequest",
                "host_local_source_id": "hermes-hook-1"
            },
            "kind": "agent_blocked",
            "severity": "action_required",
            "status": "unread",
            "title": "Approval required",
            "body": "",
            "metadata": {},
            "created_at": created_at,
            "dedupe_key": "legacy-moment"
        }
    });
    let log = format!("{legacy}\n");
    let layout = Layout::new(
        &env,
        &[
            ("format.json", b"{\"format_version\":1}\n"),
            ("notifications.jsonl", log.as_bytes()),
        ],
    );

    let mut daemon = daemon_command(&env, &layout).spawn().expect("spawn daemon");
    wait_until_ready(&mut daemon, &layout.socket()).await;
    let mut client = pohunek_client::Client::connect_local(&layout.socket())
        .await
        .expect("connect to the daemon");
    let mut projector = create_params();
    projector.source.provider = "pohunek".to_owned();
    projector.kind = NotificationKind::AgentBlocked;
    projector.dedupe_key = Some("legacy-moment".to_owned());
    let reported = client
        .call::<method::NotificationCreate>(projector)
        .await
        .expect("the format-1 store is available");
    assert_eq!(reported.record.id.0, "n-legacy", "{reported:?}");
    let listed = client
        .call::<method::NotificationList>(NotificationListParams::default())
        .await
        .expect("the store lists");
    assert_eq!(listed.notifications.len(), 1, "{listed:?}");
    assert_eq!(listed.notifications[0].source.provider, "hermes");
    drop(client);
    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;

    let marker = std::fs::read_to_string(layout.notifications.join("format.json"))
        .expect("the format marker is kept");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&marker).expect("marker json"),
        serde_json::json!({"format_version": 2})
    );
}
