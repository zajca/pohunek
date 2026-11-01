//! `policy.json` keeps the byte layout of the earlier release, so a rolled-back
//! daemon reads what this build wrote and this build leaves a document of the
//! earlier release unchanged when it persists the policy it read.

// Rust guideline compliant 2026-11-01

use std::collections::BTreeMap;

use pohunek_daemon::notifications::NotificationService;
use protocol::{NotificationKindPolicy, NotificationPolicy, NotificationRetentionPolicy};

/// The policy document the 0.38 release writes for [`policy`], as recorded in
/// its `to_writer_pretty` layout.
const RECORDED_POLICY: &str = include_str!("fixtures/notifications/policy-0.38.json");

/// Where the service keeps `policy.json` below its data directory.
fn policy_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("notifications").join("policy.json")
}

fn policy() -> NotificationPolicy {
    NotificationPolicy {
        attention_dedupe_window_secs: 90,
        attention_debounce_secs: 7,
        enabled: NotificationKindPolicy::from_flags([
            (protocol::NotificationKind::AgentBlocked, true),
            (protocol::NotificationKind::ApprovalRequired, true),
            (protocol::NotificationKind::TurnCompleted, false),
            (protocol::NotificationKind::SessionFinished, false),
            (protocol::NotificationKind::Error, true),
            (protocol::NotificationKind::System, true),
        ]),
        providers: BTreeMap::from([(
            "codex".to_owned(),
            NotificationKindPolicy::from_flags([
                (protocol::NotificationKind::AgentBlocked, true),
                (protocol::NotificationKind::ApprovalRequired, false),
                (protocol::NotificationKind::TurnCompleted, true),
                (protocol::NotificationKind::SessionFinished, false),
                (protocol::NotificationKind::Error, true),
                (protocol::NotificationKind::System, false),
            ]),
        )]),
        retention: NotificationRetentionPolicy {
            sweep_interval_secs: 60,
            info_ttl_secs: 10,
            warning_ttl_secs: 20,
            resolved_attention_ttl_secs: 30,
            resolved_error_ttl_secs: 40,
            archived_ttl_secs: 50,
            compaction_min_actions: 1,
            ..NotificationRetentionPolicy::default()
        },
        unknown_fields: protocol::tolerant::UnknownFields::default(),
    }
}

#[test]
fn the_persisted_policy_has_the_bytes_of_the_earlier_release() {
    let root = pohunek_test_support::tempdir().expect("private data root");
    let data_dir = root.path().join("data");
    let service = NotificationService::open(&data_dir).expect("open the service");

    service.set_policy(policy()).expect("persist the policy");

    let written = std::fs::read_to_string(policy_path(&data_dir)).expect("read policy.json");
    assert_eq!(written, RECORDED_POLICY);
}

#[test]
fn a_policy_document_of_the_earlier_release_is_read_and_rewritten_unchanged() {
    let root = pohunek_test_support::tempdir().expect("private data root");
    let data_dir = root.path().join("data");
    NotificationService::open(&data_dir)
        .expect("open the service")
        .set_policy(policy())
        .expect("create the store files");
    std::fs::write(policy_path(&data_dir), RECORDED_POLICY).expect("record the document");

    let service = NotificationService::open(&data_dir).expect("reopen the service");
    assert_eq!(service.policy(), policy());
    service
        .set_policy(service.policy())
        .expect("persist the policy it read");

    let rewritten = std::fs::read_to_string(policy_path(&data_dir)).expect("read policy.json");
    assert_eq!(rewritten, RECORDED_POLICY);
}
