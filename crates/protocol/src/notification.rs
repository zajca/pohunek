//! Typed payloads for the `notification.*` method family.
//!
//! Notifications are durable, per-host records surfaced by provider hooks and
//! daemon-owned projectors. These types define only the additive wire contract:
//! storage, defaults, dedupe evaluation, and retention behavior live in the
//! daemon.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::session::{AgentKind, SessionId};

/// Opaque notification identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationId.ts"))]
pub struct NotificationId(pub String);

/// High-level notification category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationKind.ts"))]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// An agent is waiting for owner attention.
    AgentBlocked,
    /// A provider approval prompt requires owner action.
    ApprovalRequired,
    /// An agent turn completed.
    TurnCompleted,
    /// A session reached a terminal lifecycle state.
    SessionFinished,
    /// A provider hook or daemon projector reported an error.
    Error,
    /// A system-level informational notification.
    System,
}

impl NotificationKind {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentBlocked => "agent_blocked",
            Self::ApprovalRequired => "approval_required",
            Self::TurnCompleted => "turn_completed",
            Self::SessionFinished => "session_finished",
            Self::Error => "error",
            Self::System => "system",
        }
    }
}

/// User-facing urgency class for a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationSeverity.ts"))]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    /// Informational notice.
    Info,
    /// Successful completion notice.
    Success,
    /// Warning notice that does not require immediate action.
    Warning,
    /// Error notice.
    Error,
    /// Action is required from the owner.
    ActionRequired,
}

impl NotificationSeverity {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::ActionRequired => "action_required",
        }
    }
}

/// Lifecycle status of a durable notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationStatus.ts"))]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    /// The owner has not opened or acted on the notification.
    Unread,
    /// The owner has read the notification.
    Read,
    /// The owner acknowledged the notification.
    Acknowledged,
    /// The owner archived the notification.
    Archived,
    /// The owner deleted the notification.
    Deleted,
}

impl NotificationStatus {
    /// Returns the stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Read => "read",
            Self::Acknowledged => "acknowledged",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }
}

/// Source that produced a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationSource.ts"))]
pub struct NotificationSource {
    /// Provider or daemon component name.
    pub provider: String,
    /// Provider event or daemon projector event name.
    pub provider_event: String,
    /// Host-local source event identifier.
    ///
    /// This identifies the sanitized local hook or projector input. It must not
    /// contain raw terminal output, prompts, secrets, environment dumps, or full
    /// tool results.
    pub host_local_source_id: String,
}

/// Durable notification record stored by one host daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationRecord.ts"))]
pub struct NotificationRecord {
    /// Stable notification id assigned by the host daemon.
    pub id: NotificationId,
    /// Sanitized source metadata for the producer.
    pub source: NotificationSource,
    /// High-level notification kind.
    pub kind: NotificationKind,
    /// User-facing severity.
    pub severity: NotificationSeverity,
    /// Current lifecycle status.
    pub status: NotificationStatus,
    /// Short user-facing title.
    pub title: String,
    /// Sanitized user-facing body.
    pub body: String,
    /// Safe producer metadata.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it. The
    /// daemon stores only allowlisted, sanitized values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// Creation timestamp in the daemon's wire timestamp format.
    pub created_at: String,
    /// Linked pohunek session id, when known.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<SessionId>,
    /// Linked agent kind, when known.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent_kind: Option<AgentKind>,
    /// Producer-specific source id, when the producer provides one.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it. This
    /// differs from [`Self::dedupe_key`], which is source-independent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub source_id: Option<String>,
    /// Source-independent dedupe key for one lifecycle group.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it. This
    /// lets provider hooks and daemon projectors meet on the same event without
    /// sharing producer-specific ids. Session-scoped keys use
    /// `attention:<session_id>` or `turn:<session_id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dedupe_key: Option<String>,
    /// Linked project id, when known.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub project_id: Option<String>,
    /// Timestamp when the owner marked the notification read.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub read_at: Option<String>,
    /// Timestamp when the owner acknowledged the notification.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub acked_at: Option<String>,
    /// Timestamp when the owner archived the notification.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub archived_at: Option<String>,
    /// Timestamp when the owner deleted the notification.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub deleted_at: Option<String>,
    /// Replacement record after lifecycle supersede processing.
    ///
    /// Additive: an older daemon omits it, and an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub superseded_by: Option<NotificationId>,
}

/// Parameters for `notification.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationCreateParams.ts"))]
pub struct NotificationCreateParams {
    /// Sanitized source metadata for the producer.
    pub source: NotificationSource,
    /// High-level notification kind.
    pub kind: NotificationKind,
    /// User-facing severity.
    pub severity: NotificationSeverity,
    /// Short user-facing title.
    pub title: String,
    /// Sanitized user-facing body.
    pub body: String,
    /// Safe producer metadata.
    ///
    /// Additive: an older client omits it, and an older daemon ignores it. The
    /// daemon stores only allowlisted, sanitized values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// Linked pohunek session id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<SessionId>,
    /// Linked agent kind, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent_kind: Option<AgentKind>,
    /// Producer-specific source id, when the producer provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub source_id: Option<String>,
    /// Source-independent dedupe key for one lifecycle group.
    ///
    /// Session-scoped keys use `attention:<session_id>` or `turn:<session_id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dedupe_key: Option<String>,
    /// Linked project id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub project_id: Option<String>,
}

/// Result returned by `notification.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationCreateResult.ts"))]
pub struct NotificationCreateResult {
    /// Whether the daemon created a new record.
    ///
    /// `false` means dedupe returned an existing visible record.
    pub created: bool,
    /// Created or existing notification record.
    pub record: NotificationRecord,
}

/// Parameters for `notification.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationListParams.ts"))]
pub struct NotificationListParams {
    /// Match [`NotificationRecord::status`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub status: Option<NotificationStatus>,
    /// Match [`NotificationRecord::kind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub kind: Option<NotificationKind>,
    /// Match [`NotificationRecord::severity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub severity: Option<NotificationSeverity>,
    /// Match [`NotificationSource::provider`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub provider: Option<String>,
    /// Match [`NotificationRecord::session_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<SessionId>,
    /// Return records created at or after this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub created_after: Option<String>,
    /// Return records created before this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub created_before: Option<String>,
    /// Maximum number of records to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub limit: Option<u32>,
    /// Pagination cursor returned by a previous list call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub cursor: Option<String>,
}

/// Result returned by `notification.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationListResult.ts"))]
pub struct NotificationListResult {
    /// Matching notification records.
    pub notifications: Vec<NotificationRecord>,
    /// Cursor for the next page, when more records are available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_cursor: Option<String>,
}

/// Parameters for `notification.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationUpdateParams.ts"))]
pub struct NotificationUpdateParams {
    /// Notification to update.
    pub id: NotificationId,
    /// New lifecycle status.
    pub status: NotificationStatus,
}

/// Result returned by `notification.update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationUpdateResult.ts"))]
pub struct NotificationUpdateResult {
    /// Updated notification record.
    pub record: NotificationRecord,
}

/// Parameters for `notification.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationDeleteParams.ts"))]
pub struct NotificationDeleteParams {
    /// Notification to delete.
    pub id: NotificationId,
}

/// Result returned by `notification.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationDeleteResult.ts"))]
pub struct NotificationDeleteResult {
    /// Deleted notification id.
    pub id: NotificationId,
    /// Whether a record was deleted.
    pub deleted: bool,
}

/// Per-kind policy flags.
#[expect(
    clippy::struct_excessive_bools,
    reason = "wire policy intentionally exposes one enable flag per notification kind"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationKindPolicy.ts"))]
#[serde(deny_unknown_fields)]
pub struct NotificationKindPolicy {
    /// Whether `agent_blocked` notifications are enabled.
    pub agent_blocked: bool,
    /// Whether `approval_required` notifications are enabled.
    pub approval_required: bool,
    /// Whether `turn_completed` notifications are enabled.
    pub turn_completed: bool,
    /// Whether `session_finished` notifications are enabled.
    pub session_finished: bool,
    /// Whether `error` notifications are enabled.
    pub error: bool,
    /// Whether `system` notifications are enabled.
    pub system: bool,
}

/// Default for [`NotificationPolicy::attention_debounce_secs`] when a persisted
/// or wire policy omits it.
///
/// Backfills policy JSON written before the field existed so an older stored
/// policy keeps loading. The daemon owns the authoritative default
/// (`DEFAULT_ATTENTION_DEBOUNCE_SECS`); this mirror only exists because the
/// protocol crate cannot depend on the daemon, and the two must stay in sync.
const fn default_attention_debounce_secs() -> u32 {
    5
}

/// Durable notification policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationPolicy.ts"))]
#[serde(deny_unknown_fields)]
pub struct NotificationPolicy {
    /// Dedupe window for equivalent attention events.
    pub attention_dedupe_window_secs: u32,
    /// Debounce window before a pending session notification may surface.
    ///
    /// Additive: an older client omits it, and a policy JSON without it loads the
    /// default so a pre-debounce persisted policy keeps working. Distinct from
    /// [`Self::attention_dedupe_window_secs`], which merges duplicate reports of
    /// one attention moment rather than delaying when it surfaces.
    #[serde(default = "default_attention_debounce_secs")]
    pub attention_debounce_secs: u32,
    /// Base per-kind policy used when a provider has no explicit entry.
    pub enabled: NotificationKindPolicy,
    /// Deterministically ordered provider-specific policy overrides.
    ///
    /// Missing keys use [`Self::enabled`]. Keys are provider wire names rather
    /// than an enum so a newly added provider does not require another public
    /// notification schema change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, NotificationKindPolicy>,
}

impl NotificationPolicy {
    /// Returns the policy for `provider`, falling back to the base policy.
    #[must_use]
    pub fn for_provider(&self, provider: &str) -> &NotificationKindPolicy {
        self.providers.get(provider).unwrap_or(&self.enabled)
    }
}

/// Parameters for `notification.policy.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationPolicyParams.ts"))]
#[serde(deny_unknown_fields)]
pub struct NotificationPolicyParams {
    /// Replacement notification policy.
    pub policy: NotificationPolicy,
}

/// Result returned by `notification.policy.get` and `notification.policy.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationPolicyResult.ts"))]
#[serde(deny_unknown_fields)]
pub struct NotificationPolicyResult {
    /// Current notification policy.
    pub policy: NotificationPolicy,
}

/// Parameters for `notification.retention.prune`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "NotificationRetentionParams.ts")
)]
pub struct NotificationRetentionParams {
    /// Whether to report matches without deleting them.
    #[serde(default)]
    pub dry_run: bool,
    /// Restrict pruning to this lifecycle status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub status: Option<NotificationStatus>,
    /// Prune records created before this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub before: Option<String>,
    /// Maximum number of records to prune.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub limit: Option<u32>,
}

/// Result returned by `notification.retention.prune`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "NotificationRetentionResult.ts")
)]
pub struct NotificationRetentionResult {
    /// Whether the daemon reported matches without deleting them.
    pub dry_run: bool,
    /// Notification ids matched or pruned by the operation.
    pub pruned: Vec<NotificationId>,
}

/// Payload for a `notification_created` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationCreatedEvent.ts"))]
pub struct NotificationCreatedEvent {
    /// Created notification record.
    pub record: NotificationRecord,
}

/// Payload for a `notification_updated` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationUpdatedEvent.ts"))]
pub struct NotificationUpdatedEvent {
    /// Updated notification record.
    pub record: NotificationRecord,
}

/// Payload for a `notification_deleted` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "NotificationDeletedEvent.ts"))]
pub struct NotificationDeletedEvent {
    /// Deleted notification id.
    pub notification_id: NotificationId,
}
