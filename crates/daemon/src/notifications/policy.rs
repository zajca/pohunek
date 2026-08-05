//! Notification policy defaults and retention evaluation.

use std::collections::BTreeMap;

use protocol::{
    NotificationId, NotificationKind, NotificationKindPolicy, NotificationPolicy,
    NotificationRecord, NotificationRetentionParams, NotificationStatus,
};

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
#[must_use]
pub fn default_policy() -> NotificationPolicy {
    let provider_policy = default_enabled_kinds();
    NotificationPolicy {
        attention_dedupe_window_secs: DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS,
        attention_debounce_secs: DEFAULT_ATTENTION_DEBOUNCE_SECS,
        enabled: default_enabled_kinds(),
        providers: BTreeMap::from([
            ("claude".to_owned(), provider_policy.clone()),
            ("codex".to_owned(), provider_policy.clone()),
            ("hermes".to_owned(), provider_policy),
        ]),
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

fn default_enabled_kinds() -> NotificationKindPolicy {
    NotificationKindPolicy {
        agent_blocked: true,
        approval_required: true,
        turn_completed: false,
        session_finished: false,
        error: true,
        system: false,
    }
}

fn kind_enabled(policy: &NotificationKindPolicy, kind: NotificationKind) -> bool {
    match kind {
        NotificationKind::AgentBlocked => policy.agent_blocked,
        NotificationKind::ApprovalRequired => policy.approval_required,
        NotificationKind::TurnCompleted => policy.turn_completed,
        NotificationKind::SessionFinished => policy.session_finished,
        NotificationKind::Error => policy.error,
        NotificationKind::System => policy.system,
    }
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

fn retention_limit(params: &NotificationRetentionParams) -> usize {
    params
        .limit
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX)
}
