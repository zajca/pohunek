//! Records a newer daemon wrote with an enum value this build does not know,
//! read after a rollback through the public daemon boundaries: the notification
//! store, the metadata store and the host governance record. Each decode stays
//! strict, so the value is refused and the file is left as it was; the
//! notification store then reports itself unavailable instead of failing to open.

// Rust guideline compliant 2026-10-28

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use pohunek_daemon::host_state::{HostStateDir, HostStateRepository};
use pohunek_daemon::notifications::NotificationService;
use pohunek_daemon::store::{ProjectRecord, SchemaMigration, Store};
use protocol::{
    EnrollmentInfo, EnrollmentRevision, EnrollmentStatus, HostOwner, NotificationCreateParams,
    NotificationKind, NotificationSeverity, NotificationSource, OwnerRevision, PrincipalId,
    RelayId,
};
use serde_json::{json, Value};

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

const NEWER_VALUE: &str = "a_value_from_a_newer_daemon";

fn replace_once(path: &Path, from: &str, to: &str) -> String {
    let stored = std::fs::read_to_string(path).expect("read the file");
    assert_eq!(stored.matches(from).count(), 1, "{from} in {stored}");
    let rewritten = stored.replacen(from, to, 1);
    std::fs::write(path, &rewritten).expect("rewrite the file");
    rewritten
}

#[test]
fn a_notification_store_line_with_an_unknown_severity_stops_replay_and_stays_on_disk() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let service = NotificationService::open(root.path()).expect("open the notification store");
    service
        .create(NotificationCreateParams {
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: "codex-hook-1".to_owned(),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::ActionRequired,
            title: "Approval required".to_owned(),
            body: "Codex is waiting for owner approval.".to_owned(),
            metadata: std::collections::BTreeMap::new(),
            session_id: None,
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            retryable_admission: false,
        })
        .expect("create a notification");
    let store_path = service.store_path().to_path_buf();
    drop(service);
    NotificationService::open(root.path()).expect("the unmodified store replays");

    let rewritten = replace_once(
        &store_path,
        "\"severity\":\"action_required\"",
        &format!("\"severity\":\"{NEWER_VALUE}\""),
    );

    let reopened = NotificationService::open(root.path())
        .expect("a store this build cannot decode opens as unavailable");
    let error = reopened
        .list(protocol::NotificationListParams::default())
        .expect_err("an unavailable store refuses to list");
    assert_eq!(
        error.to_protocol_error().code,
        "notifications_unavailable",
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(&store_path).expect("read the store again"),
        rewritten,
        "the refused store is left untouched"
    );
}

fn fixture_store(root: &Path) -> (Store, std::path::PathBuf) {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-10 store")
        .write_all(SCHEMA_10.as_bytes())
        .expect("write frozen schema-10 store");
    let store = Store::new(path.clone());
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));
    (store, path)
}

#[test]
fn a_stored_project_with_an_unknown_source_is_rejected_and_kept_on_disk() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let project: ProjectRecord = serde_json::from_value(json!({
        "git_common_dir": "/repo/.git",
        "repo_root": "/repo",
        "source": "manual",
        "added_at": "2026-01-01T00:00:00Z",
        "last_used_at": "2026-01-01T00:00:01Z",
    }))
    .expect("project record");
    store
        .mutate_project(Path::new("/repo/.git"), |_| Some(project))
        .expect("write a project");
    assert_eq!(store.load_projects().expect("load projects").len(), 1);

    let rewritten = replace_once(
        &path,
        "\"source\":\"manual\"",
        &format!("\"source\":\"{NEWER_VALUE}\""),
    );

    assert!(
        store.load_projects().expect("load projects").is_empty(),
        "the store decode stays strict"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the store again"),
        rewritten,
        "the rejected line stays on disk for the daemon that wrote it"
    );
}

/// Rewrites the never-enrolled governance record of `state_dir` into a
/// quarantined one that names `reason`.
fn quarantine_governance(state_dir: &Path, reason: &str) {
    let identifier = |prefix: &str| format!("{prefix}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let dir = HostStateDir::open_existing(state_dir).expect("open the host state directory");
    let record = dir
        .read_record("governance.json")
        .expect("read the governance record")
        .expect("the governance record exists");
    let mut governance: Value = serde_json::from_slice(&record).expect("governance JSON");
    governance["enrollment"] = serde_json::to_value(EnrollmentInfo::new(
        RelayId::parse(&identifier("relay_")).expect("relay id"),
        EnrollmentStatus::Quarantined,
        EnrollmentRevision::new(2).expect("revision"),
    ))
    .expect("enrollment JSON");
    governance["owner"] = serde_json::to_value(HostOwner::Principal(
        PrincipalId::parse(&identifier("principal_")).expect("principal id"),
    ))
    .expect("owner JSON");
    governance["owner_revision"] =
        serde_json::to_value(OwnerRevision::new(3).expect("owner revision"))
            .expect("owner revision JSON");
    governance["quarantine"] = json!(reason);
    governance["quarantine_origin"] = json!("active");
    dir.replace_record(
        "governance.json",
        &serde_json::to_vec(&governance).expect("governance bytes"),
    )
    .expect("write the governance record");
}

#[test]
fn a_governance_record_with_an_unknown_quarantine_reason_is_refused() {
    let known = pohunek_test_support::tempdir().expect("private fixture root");
    HostStateRepository::open_or_create(known.path()).expect("bootstrap host state");
    quarantine_governance(known.path(), "projection_conflict");
    HostStateRepository::open_or_create(known.path())
        .expect("a record with a known reason is accepted");

    let newer = pohunek_test_support::tempdir().expect("private fixture root");
    HostStateRepository::open_or_create(newer.path()).expect("bootstrap host state");
    quarantine_governance(newer.path(), NEWER_VALUE);
    HostStateRepository::open_or_create(newer.path())
        .expect_err("governance stays fail-closed on a reason this build does not know");
}
