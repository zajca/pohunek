//! Notification policy defaults and retention evaluation.

use std::collections::BTreeMap;

use protocol::{
    NotificationId, NotificationKind, NotificationKindPolicy, NotificationPolicy,
    NotificationRecord, NotificationRetentionParams, NotificationRetentionPolicy,
    NotificationSeverity, NotificationStatus,
};
use time::{Duration, OffsetDateTime};

use super::{parse_timestamp, NotificationError};

/// Default attention dedupe window in seconds.
///
/// Provider hooks and daemon projectors often report the same approval/blocking
/// moment within a few UI polling cycles. Two minutes covers normal scheduling
/// delay without merging unrelated later turns.
pub const DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS: u32 = 120;

/// Default debounce window before a pending session notification may surface.
///
/// An attention state (`agent_blocked` / `approval_required`) or completed turn
/// that resolves within this window never produces a durable record or an OS/GUI
/// notification, so transient prompts an agent answers itself do not flash at the
/// owner. Five seconds is long enough to swallow those self-resolving blips while
/// keeping a genuine wait perceptibly prompt. Distinct from
/// [`DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS`], which merges duplicate reports of one
/// attention moment across producers rather than delaying when it surfaces. Must
/// stay in sync with the protocol crate's `default_attention_debounce_secs`
/// serde backfill.
pub const DEFAULT_ATTENTION_DEBOUNCE_SECS: u32 = 5;

/// Default policy for daemon notification creation.
///
/// It has no per-provider overrides: every provider, whatever runtime it
/// names, is governed by the base `enabled` flags until an operator sets an
/// override for it.
#[must_use]
pub fn default_policy() -> NotificationPolicy {
    NotificationPolicy {
        attention_dedupe_window_secs: DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS,
        attention_debounce_secs: DEFAULT_ATTENTION_DEBOUNCE_SECS,
        enabled: default_enabled_kinds(),
        providers: BTreeMap::new(),
        retention: NotificationRetentionPolicy::default(),
        unknown_fields: protocol::tolerant::UnknownFields::default(),
    }
}

/// Return whether `kind` is enabled for `provider`.
#[must_use]
pub fn policy_enables_kind(
    policy: &NotificationPolicy,
    provider: &str,
    kind: NotificationKind,
) -> bool {
    let normalized_provider = provider.to_ascii_lowercase();
    let kinds = policy.for_provider(&normalized_provider);
    kind_enabled(kinds, kind)
}

/// The daemon's default flag for `kind`. The unknown kind is never enabled; the
/// daemon never holds it.
const fn default_kind_enabled(kind: NotificationKind) -> bool {
    match kind {
        NotificationKind::AgentBlocked
        | NotificationKind::ApprovalRequired
        | NotificationKind::Error => true,
        NotificationKind::TurnCompleted
        | NotificationKind::SessionFinished
        | NotificationKind::System
        | NotificationKind::Unknown => false,
    }
}

fn default_enabled_kinds() -> NotificationKindPolicy {
    NotificationKindPolicy::from_fn(default_kind_enabled)
}

/// Gives every known kind of the base policy and of each provider override a
/// flag, the daemon's default where the document has none. A policy is stored
/// and returned complete, so a reader of the earlier release, which requires
/// every flag, decodes what this daemon wrote.
pub(crate) fn complete_kind_flags(policy: &mut NotificationPolicy) {
    let overrides = policy.providers.values_mut();
    for flags in std::iter::once(&mut policy.enabled).chain(overrides) {
        for kind in NotificationKind::KNOWN {
            if flags.get(kind).is_none() {
                flags.set(kind, default_kind_enabled(kind));
            }
        }
    }
}

/// The flag `policy` holds for `kind`, or the daemon's default for a kind the
/// document leaves out.
fn kind_enabled(policy: &NotificationKindPolicy, kind: NotificationKind) -> bool {
    policy.get(kind).unwrap_or(default_kind_enabled(kind))
}

pub(crate) fn records_matching_retention(
    records: &[NotificationRecord],
    params: &NotificationRetentionParams,
) -> Result<Vec<NotificationId>, NotificationError> {
    let before = params.before.as_deref().map(parse_timestamp).transpose()?;
    let limit = retention_limit(params);
    let mut ids = Vec::new();
    for record in records {
        if ids.len() >= limit {
            break;
        }
        if record.status == NotificationStatus::Deleted && params.status.is_none() {
            continue;
        }
        let created_at = parse_timestamp(&record.created_at)?;
        if params.status.is_none_or(|wanted| record.status == wanted)
            && before.is_none_or(|before| created_at < before)
        {
            ids.push(record.id.clone());
        }
    }
    Ok(ids)
}

/// Return records eligible for the automatic age-based retention sweep.
///
/// Active action-required and error records deliberately have no TTL. Reading
/// a notification is presentation state only; acknowledgement or archival is
/// required before an actionable incident can age out.
pub(crate) fn records_matching_auto_retention(
    records: &[NotificationRecord],
    retention: &NotificationRetentionPolicy,
    now: OffsetDateTime,
) -> Result<Vec<NotificationId>, NotificationError> {
    let mut ids = Vec::new();
    for record in records {
        if record.status == NotificationStatus::Deleted {
            continue;
        }
        if auto_retention_deadline(record, retention)?.is_some_and(|deadline| deadline <= now) {
            ids.push(record.id.clone());
        }
    }
    Ok(ids)
}

fn auto_retention_deadline(
    record: &NotificationRecord,
    retention: &NotificationRetentionPolicy,
) -> Result<Option<OffsetDateTime>, NotificationError> {
    let (timestamp, ttl_secs) = match record.status {
        NotificationStatus::Archived => (
            record.archived_at.as_deref().unwrap_or(&record.created_at),
            retention.archived_ttl_secs,
        ),
        NotificationStatus::Acknowledged
            if matches!(
                record.severity,
                NotificationSeverity::ActionRequired | NotificationSeverity::Error
            ) || matches!(
                record.kind,
                NotificationKind::AgentBlocked
                    | NotificationKind::ApprovalRequired
                    | NotificationKind::Error
            ) =>
        {
            let ttl = if record.severity == NotificationSeverity::Error
                || record.kind == NotificationKind::Error
            {
                retention.resolved_error_ttl_secs
            } else {
                retention.resolved_attention_ttl_secs
            };
            (
                record.acked_at.as_deref().unwrap_or(&record.created_at),
                ttl,
            )
        }
        NotificationStatus::Unread | NotificationStatus::Read
            if matches!(
                record.severity,
                NotificationSeverity::ActionRequired | NotificationSeverity::Error
            ) || matches!(
                record.kind,
                NotificationKind::AgentBlocked
                    | NotificationKind::ApprovalRequired
                    | NotificationKind::Error
            ) =>
        {
            return Ok(None);
        }
        NotificationStatus::Unread
        | NotificationStatus::Read
        | NotificationStatus::Acknowledged => {
            let ttl = if record.severity == NotificationSeverity::Warning {
                retention.warning_ttl_secs
            } else {
                retention.info_ttl_secs
            };
            (record.created_at.as_str(), ttl)
        }
        // The daemon never holds an unknown status; a record it cannot classify is kept.
        NotificationStatus::Deleted | NotificationStatus::Unknown => return Ok(None),
    };
    Ok(Some(
        parse_timestamp(timestamp)? + Duration::seconds(i64::from(ttl_secs)),
    ))
}

fn retention_limit(params: &NotificationRetentionParams) -> usize {
    params
        .limit
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::{
        NotificationId, NotificationKind, NotificationRecord, NotificationRetentionPolicy,
        NotificationSeverity, NotificationSource, NotificationStatus,
    };
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    use super::records_matching_auto_retention;

    fn timestamp(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).expect("valid test timestamp")
    }

    fn record(
        id: &str,
        kind: NotificationKind,
        severity: NotificationSeverity,
        status: NotificationStatus,
    ) -> NotificationRecord {
        NotificationRecord {
            id: NotificationId(id.to_owned()),
            source: NotificationSource {
                provider: "pohunek".to_owned(),
                provider_event: "test".to_owned(),
                host_local_source_id: format!("source-{id}"),
            },
            kind,
            severity,
            status,
            title: id.to_owned(),
            body: id.to_owned(),
            metadata: BTreeMap::new(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            session_id: None,
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            read_at: None,
            acked_at: (status == NotificationStatus::Acknowledged)
                .then(|| "2026-01-02T00:00:00Z".to_owned()),
            archived_at: (status == NotificationStatus::Archived)
                .then(|| "2026-01-02T00:00:00Z".to_owned()),
            deleted_at: None,
            superseded_by: None,
            turn_started_at: None,
            turn_duration_ms: None,
            session_metadata: BTreeMap::new(),
            hook_admitted: false,
        }
    }

    #[test]
    fn automatic_retention_never_expires_unresolved_actions_or_errors() {
        let records = vec![
            record(
                "action",
                NotificationKind::ApprovalRequired,
                NotificationSeverity::ActionRequired,
                NotificationStatus::Unread,
            ),
            record(
                "error",
                NotificationKind::Error,
                NotificationSeverity::Error,
                NotificationStatus::Read,
            ),
        ];

        let matched = records_matching_auto_retention(
            &records,
            &NotificationRetentionPolicy::default(),
            timestamp("2030-01-01T00:00:00Z"),
        )
        .expect("evaluate retention");

        assert!(matched.is_empty());
    }

    #[test]
    fn automatic_retention_uses_activity_resolution_and_archive_ttls() {
        let records = vec![
            record(
                "info",
                NotificationKind::TurnCompleted,
                NotificationSeverity::Success,
                NotificationStatus::Unread,
            ),
            record(
                "warning",
                NotificationKind::System,
                NotificationSeverity::Warning,
                NotificationStatus::Read,
            ),
            record(
                "resolved-action",
                NotificationKind::AgentBlocked,
                NotificationSeverity::ActionRequired,
                NotificationStatus::Acknowledged,
            ),
            record(
                "resolved-error",
                NotificationKind::Error,
                NotificationSeverity::Error,
                NotificationStatus::Acknowledged,
            ),
            record(
                "archived",
                NotificationKind::System,
                NotificationSeverity::Info,
                NotificationStatus::Archived,
            ),
        ];
        let retention = NotificationRetentionPolicy {
            sweep_interval_secs: 60,
            info_ttl_secs: 10,
            warning_ttl_secs: 20,
            resolved_attention_ttl_secs: 30,
            resolved_error_ttl_secs: 40,
            archived_ttl_secs: 50,
            compaction_min_actions: 1,
            ..Default::default()
        };

        let matched = records_matching_auto_retention(
            &records,
            &retention,
            timestamp("2026-01-02T00:01:00Z"),
        )
        .expect("evaluate retention");

        assert_eq!(
            matched,
            vec![
                NotificationId("info".to_owned()),
                NotificationId("warning".to_owned()),
                NotificationId("resolved-action".to_owned()),
                NotificationId("resolved-error".to_owned()),
                NotificationId("archived".to_owned()),
            ]
        );
    }
}
