//! A kind the policy document leaves out takes the daemon's default for that
//! kind, whether the document comes from `policy.json` or from the params of
//! `notification.policy.set`.

// Rust guideline compliant 2026-11-01

use pohunek_daemon::notifications::{policy_enables_kind, NotificationService};
use protocol::{NotificationKind, NotificationPolicyParams};
use serde_json::json;

/// Where the service keeps `policy.json` below its data directory.
fn policy_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("notifications").join("policy.json")
}

/// The daemon's default for each kind, for a provider with no override.
const DEFAULTS: [(NotificationKind, bool); 6] = [
    (NotificationKind::AgentBlocked, true),
    (NotificationKind::ApprovalRequired, true),
    (NotificationKind::TurnCompleted, false),
    (NotificationKind::SessionFinished, false),
    (NotificationKind::Error, true),
    (NotificationKind::System, false),
];

fn assert_defaults_except(
    service: &NotificationService,
    provider: &str,
    exceptions: &[(NotificationKind, bool)],
) {
    let policy = service.policy();
    for (kind, default) in DEFAULTS {
        let expected = exceptions
            .iter()
            .find(|(excepted, _)| *excepted == kind)
            .map_or(default, |(_, enabled)| *enabled);
        assert_eq!(
            policy_enables_kind(&policy, provider, kind),
            expected,
            "{provider}: {}",
            kind.as_str()
        );
    }
}

#[test]
fn a_kind_missing_from_policy_json_takes_the_daemon_default() {
    let root = pohunek_test_support::tempdir().expect("private data root");
    let data_dir = root.path().join("data");
    NotificationService::open(&data_dir)
        .expect("open the service")
        .set_policy(pohunek_daemon::notifications::default_policy())
        .expect("create the store files");
    let document = json!({
        "attention_dedupe_window_secs": 300,
        "enabled": {"turn_completed": true, "error": false},
        "providers": {"codex": {}},
    });
    std::fs::write(policy_path(&data_dir), format!("{document}\n")).expect("write policy.json");

    let service = NotificationService::open(&data_dir).expect("reopen the service");

    let base = [
        (NotificationKind::TurnCompleted, true),
        (NotificationKind::Error, false),
    ];
    assert_defaults_except(&service, "claude", &base);
    assert_defaults_except(&service, "codex", &[]);
}

#[test]
fn a_kind_missing_from_policy_params_takes_the_daemon_default() {
    let root = pohunek_test_support::tempdir().expect("private data root");
    let data_dir = root.path().join("data");
    let service = NotificationService::open(&data_dir).expect("open the service");
    let params: NotificationPolicyParams = serde_json::from_value(json!({
        "policy": {
            "attention_dedupe_window_secs": 300,
            "enabled": {"system": true},
            "providers": {"codex": {"agent_blocked": false}},
        }
    }))
    .expect("the daemon decodes params that leave kinds out");

    service
        .set_policy(params.policy)
        .expect("persist the policy");

    let stored = service.policy();
    for flags in std::iter::once(&stored.enabled).chain(stored.providers.values()) {
        for kind in NotificationKind::KNOWN {
            assert!(
                flags.get(kind).is_some(),
                "a stored policy is complete: {}",
                kind.as_str()
            );
        }
    }

    assert_defaults_except(&service, "claude", &[(NotificationKind::System, true)]);
    assert_defaults_except(
        &service,
        "codex",
        &[(NotificationKind::AgentBlocked, false)],
    );
}
