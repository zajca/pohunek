//! Guard against notification store changes that skip a format bump.
//!
//! The recursive key set and the serialized enum value sets of the action log
//! lines (`StoreAction`, `NotificationRecord`) and of `policy.json`
//! (`NotificationPolicy`) are compared with the checked-in snapshot
//! `fixtures/format-<N>.txt` for the current [`NOTIFICATIONS_FORMAT_VERSION`].
//! A build meets a key or value it does not know only in a store a newer build
//! wrote, and then declines to use the store instead of reading part of it, so
//! every addition must come with a format bump. Every enum is listed through
//! `unit_values!` or `data_values!` and every string leaf must be a registered
//! enum path or free text, as in the store guard; the helpers are the
//! `pohunek-shape-guard` crate.
//!
//! When the snapshot no longer matches, the test fails. To resolve it:
//!
//! 1. bump [`NOTIFICATIONS_FORMAT_VERSION`],
//! 2. make the reader accept the previous format as well,
//! 3. add `fixtures/format-<new>.txt` with the content the failure prints and
//!    register it in the snapshot table.

use std::collections::BTreeSet;

use persisted_shape::{
    check_snapshot, classify_leaves, collect_paths, collect_string_leaves, data_values,
    unit_values, EnumEntry, Leaves, SnapshotGuard,
};
use protocol::{
    NotificationKind, NotificationKindPolicy, NotificationPolicy, NotificationRecord,
    NotificationRetentionPolicy, NotificationSeverity, NotificationSource, NotificationStatus,
};
use serde_json::{json, Value};

use super::{StoreAction, NOTIFICATIONS_FORMAT_VERSION};

/// Key and value snapshots by format version.
const SNAPSHOTS: &[(u32, &str)] = &[(1, include_str!("../fixtures/format-1.txt"))];

/// Locations of a record inside a serialized action.
const RECORD_ROOTS: &[&str] = &["store.record", "store.updated[]"];

/// A record with every optional field populated.
fn full_record() -> NotificationRecord {
    serde_json::from_value(json!({
        "id": "n-1",
        "source": {
            "provider": "codex",
            "provider_event": "PermissionRequest",
            "host_local_source_id": "codex-hook-1"
        },
        "kind": "approval_required",
        "severity": "action_required",
        "status": "read",
        "title": "Approval required",
        "body": "Codex is waiting for owner approval.",
        "metadata": {"channel": "desktop"},
        "created_at": "2026-10-29T10:00:00Z",
        "session_id": "s-1",
        "agent_kind": "codex",
        "source_id": "turn:1",
        "dedupe_key": "attention:s-1",
        "project_id": "project-1",
        "read_at": "2026-10-29T10:01:00Z",
        "acked_at": "2026-10-29T10:02:00Z",
        "archived_at": "2026-10-29T10:03:00Z",
        "deleted_at": "2026-10-29T10:04:00Z",
        "superseded_by": "n-2",
        "turn_started_at": "2026-10-29T09:59:00Z",
        "turn_duration_ms": 61000,
        "session_metadata": {"purpose": "review"}
    }))
    .expect("full record")
}

/// Fails to compile when a field is added to a stored type, including an
/// optional field that stays out of the serialized samples while it is `None`.
#[expect(
    clippy::unneeded_field_pattern,
    reason = "naming every field with `_` is what makes a field change a compile error"
)]
fn assert_every_field_is_reviewed(record: &NotificationRecord, policy: &NotificationPolicy) {
    let NotificationRecord {
        id: _,
        source:
            NotificationSource {
                provider: _,
                provider_event: _,
                host_local_source_id: _,
            },
        kind: _,
        severity: _,
        status: _,
        title: _,
        body: _,
        metadata: _,
        created_at: _,
        session_id: _,
        agent_kind: _,
        source_id: _,
        dedupe_key: _,
        project_id: _,
        read_at: _,
        acked_at: _,
        archived_at: _,
        deleted_at: _,
        superseded_by: _,
        turn_started_at: _,
        turn_duration_ms: _,
        session_metadata: _,
    } = record;
    let NotificationPolicy {
        attention_dedupe_window_secs: _,
        attention_debounce_secs: _,
        enabled:
            NotificationKindPolicy {
                agent_blocked: _,
                approval_required: _,
                turn_completed: _,
                session_finished: _,
                error: _,
                system: _,
            },
        providers: _,
        retention:
            NotificationRetentionPolicy {
                sweep_interval_secs: _,
                info_ttl_secs: _,
                warning_ttl_secs: _,
                resolved_attention_ttl_secs: _,
                resolved_error_ttl_secs: _,
                archived_ttl_secs: _,
                compaction_min_actions: _,
            },
    } = policy;
}

fn actions() -> Vec<StoreAction> {
    let record = full_record();
    vec![
        StoreAction::Created {
            record: record.clone(),
        },
        StoreAction::CreatedWithUpdates {
            record: record.clone(),
            updated: vec![record.clone()],
        },
        StoreAction::Updated {
            record: record.clone(),
        },
        StoreAction::Deleted { record },
    ]
}

fn action_values() -> Vec<String> {
    let record = full_record();
    data_values!(
        StoreAction, Some("action");
        StoreAction::Created { .. } => StoreAction::Created { record: record.clone() },
        StoreAction::CreatedWithUpdates { .. } => StoreAction::CreatedWithUpdates {
            record: record.clone(),
            updated: vec![record.clone()],
        },
        StoreAction::Updated { .. } => StoreAction::Updated { record: record.clone() },
        StoreAction::Deleted { .. } => StoreAction::Deleted { record: record.clone() },
    )
}

fn at_records(relative: &str) -> Vec<String> {
    RECORD_ROOTS
        .iter()
        .map(|root| format!("{root}.{relative}"))
        .collect()
}

fn enums() -> Vec<EnumEntry> {
    vec![
        EnumEntry {
            paths: vec!["store.action".to_owned()],
            values: action_values(),
        },
        EnumEntry {
            paths: at_records("kind"),
            values: unit_values!(
                NotificationKind: AgentBlocked, ApprovalRequired, TurnCompleted, SessionFinished,
                Error, System
            ),
        },
        EnumEntry {
            paths: at_records("severity"),
            values: unit_values!(
                NotificationSeverity: Info, Success, Warning, Error, ActionRequired; skip Unknown
            ),
        },
        EnumEntry {
            paths: at_records("status"),
            values: unit_values!(
                NotificationStatus: Unread, Read, Acknowledged, Archived, Deleted; skip Unknown
            ),
        },
    ]
}

/// Identifiers, producer-chosen strings, user metadata (`metadata.*`) and
/// timestamps.
fn free_text() -> Vec<Vec<String>> {
    [
        "id",
        "source.provider",
        "source.provider_event",
        "source.host_local_source_id",
        "title",
        "body",
        "metadata.*",
        "created_at",
        "session_id",
        "agent_kind",
        "source_id",
        "dedupe_key",
        "project_id",
        "read_at",
        "acked_at",
        "archived_at",
        "deleted_at",
        "superseded_by",
        "turn_started_at",
        "session_metadata.purpose",
    ]
    .iter()
    .map(|relative| at_records(relative))
    .collect()
}

/// Collapses the keys of an open map to `*`, so a sample key is not pinned.
fn collapse_maps(path: &str) -> String {
    let segments: Vec<&str> = path.split('.').collect();
    for map in ["metadata", "session_metadata", "providers"] {
        if let Some(index) = segments.iter().position(|segment| *segment == map) {
            if index + 1 < segments.len() {
                let mut collapsed = segments[..=index].join(".");
                collapsed.push_str(".*");
                if index + 2 < segments.len() {
                    collapsed.push('.');
                    collapsed.push_str(&segments[index + 2..].join("."));
                }
                return collapsed;
            }
        }
    }
    path.to_owned()
}

/// The sample policy: every key populated, with one provider override.
fn policy_document() -> Value {
    let mut policy = crate::notifications::default_policy();
    policy
        .providers
        .insert("codex".to_owned(), policy.enabled.clone());
    serde_json::to_value(policy).expect("policy json")
}

/// Key and `path=value` lines of the notification store.
fn current_shape() -> BTreeSet<String> {
    assert_every_field_is_reviewed(&full_record(), &crate::notifications::default_policy());
    let mut shape = BTreeSet::new();
    let mut leaves = Leaves::new();
    for action in actions() {
        let document = serde_json::to_value(&action).expect("action json");
        let mut paths = BTreeSet::new();
        collect_paths("", &document, &mut paths);
        shape.extend(
            paths
                .into_iter()
                .map(|path| collapse_maps(&format!("store.{path}"))),
        );
        let mut found = Leaves::new();
        collect_string_leaves("", &document, &mut found);
        for (path, values) in found {
            leaves
                .entry(format!("store.{path}"))
                .or_default()
                .extend(values);
        }
    }
    let mut paths = BTreeSet::new();
    collect_paths("", &policy_document(), &mut paths);
    shape.extend(
        paths
            .into_iter()
            .map(|path| collapse_maps(&format!("policy.{path}"))),
    );
    shape.insert("store".to_owned());
    shape.insert("policy".to_owned());
    let mut found = Leaves::new();
    collect_string_leaves("", &policy_document(), &mut found);
    assert!(
        found.is_empty(),
        "policy.json gained a string leaf: classify it in free_text or enums: {found:?}"
    );
    shape.extend(
        classify_leaves(&leaves, &enums(), &free_text())
            .unwrap_or_else(|message| panic!("{message}")),
    );
    shape
}

/// Compares `actual` with the snapshot kept for `version`.
fn check_format(version: u32, actual: &BTreeSet<String>) -> Result<(), String> {
    check_snapshot(
        &SnapshotGuard {
            record: "notification store",
            bump: "format",
            constant: "NOTIFICATIONS_FORMAT_VERSION",
            fixture: "crates/daemon/src/notifications/fixtures/format-<N>.txt",
            follow_up: "make the reader accept the previous format as well",
        },
        version,
        actual,
        SNAPSHOTS,
    )
}

#[test]
fn the_notification_store_matches_the_snapshot_of_the_current_format() {
    if let Err(message) = check_format(NOTIFICATIONS_FORMAT_VERSION, &current_shape()) {
        panic!("{message}");
    }
}

#[test]
fn a_changed_notification_shape_is_reported_as_an_unformatted_change() {
    let mut shape = current_shape();
    shape.insert("store.record.status=suspended".to_owned());
    let message = check_format(NOTIFICATIONS_FORMAT_VERSION, &shape)
        .expect_err("an added enum value must fail the guard");
    assert!(message.contains("without a format bump"), "{message}");
    assert!(message.contains("accept the previous format"), "{message}");
    assert!(
        message.contains("store.record.status=suspended"),
        "{message}"
    );

    let mut shape = current_shape();
    shape.insert("policy.new_key".to_owned());
    check_format(NOTIFICATIONS_FORMAT_VERSION, &shape)
        .expect_err("an added policy key must fail the guard");
}

#[test]
fn a_bumped_notification_format_without_a_snapshot_is_reported() {
    let message = check_format(NOTIFICATIONS_FORMAT_VERSION + 1, &current_shape())
        .expect_err("a new format needs its own snapshot");
    assert!(message.contains("has no snapshot"), "{message}");
}
