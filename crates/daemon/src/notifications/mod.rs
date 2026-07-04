//! Durable daemon notification service.

mod policy;
mod projector;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use protocol::{
    event, ErrorClass, Event, NotificationCreateParams, NotificationCreateResult,
    NotificationCreatedEvent, NotificationDeleteParams, NotificationDeleteResult,
    NotificationDeletedEvent, NotificationId, NotificationKind, NotificationListParams,
    NotificationListResult, NotificationPolicy, NotificationRecord, NotificationRetentionParams,
    NotificationRetentionResult, NotificationSource, NotificationStatus, NotificationUpdateParams,
    NotificationUpdateResult, NotificationUpdatedEvent, ProtocolError,
};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::broadcast;

#[doc(inline)]
pub use policy::{default_policy, policy_enables_kind, DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS};
#[doc(inline)]
pub use projector::{attention_dedupe_key, NotificationProjector};
#[doc(inline)]
pub use store::NOTIFICATIONS_SUBDIR;

pub(crate) use store::NotificationStore;

/// Maximum title length stored for user-controlled notification titles.
///
/// Native notification surfaces and compact inbox rows truncate aggressively, so
/// storing more title text only increases durable payload size without improving
/// operator decisions.
pub const MAX_TITLE_CHARS: usize = 160;

/// Maximum body length stored for user-controlled notification bodies.
///
/// The body is summary text, never raw terminal output or tool logs. The limit
/// keeps JSONL records bounded while leaving enough room for useful context.
pub const MAX_BODY_CHARS: usize = 4_096;

/// Maximum source field length stored in durable notification records.
///
/// Source fields identify a sanitized producer namespace, not arbitrary payload.
/// Bounding them prevents hook input from becoming an unbounded store key.
const MAX_SOURCE_FIELD_CHARS: usize = 256;

/// Maximum linked pohunek session id length stored in notification records.
///
/// Matches the existing id-kind native session reference cap so hook-supplied
/// session references cannot become unbounded durable store keys.
const MAX_NOTIFICATION_SESSION_ID_BYTES: usize = 512;

/// Maximum dedupe key length stored in durable notification records.
///
/// Dedupe keys are stable attention identifiers. This is enough for
/// session/project/source tuples without allowing unbounded hook payloads.
const MAX_DEDUPE_KEY_CHARS: usize = 512;

/// Maximum metadata entries accepted from notification producers.
///
/// Metadata is intended for a small set of routing/display tags. Eight entries
/// covers the shipped allowlist combinations without allowing arbitrary payloads
/// to become durable notification state.
pub const MAX_METADATA_ENTRIES: usize = 8;

/// Maximum metadata value length stored in durable notification records.
///
/// Metadata values are compact tags, URLs, or short summaries. Larger content is
/// likely raw hook output and belongs outside the notification store.
pub const MAX_METADATA_VALUE_CHARS: usize = 512;

/// Maximum project id length stored in durable notification records.
///
/// Project ids are compact daemon-generated identifiers, so larger values are
/// user input that should be bounded before reaching the store.
const MAX_PROJECT_ID_CHARS: usize = 128;

/// Capacity for notification subscription events.
///
/// Matches the session event channel scale so short subscriber stalls do not
/// drop ordinary notification bursts, while keeping per-daemon memory bounded.
const NOTIFICATION_EVENT_CAPACITY: usize = 128;

/// Secret-like metadata keys rejected before notification storage.
const SECRET_METADATA_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "api_key",
    "authorization",
    "cookie",
];

/// Safe metadata keys accepted from notification producers.
const ALLOWED_METADATA_KEYS: &[&str] = &[
    "action_url",
    "detail_url",
    "provider",
    "provider_event",
    "reason",
    "summary",
    "hook_event_id",
    "matcher",
    "tool_name",
];

/// Error returned by the daemon notification subsystem.
#[derive(Debug, Error)]
pub enum NotificationError {
    /// The append-only store could not be read or written.
    #[error("notification store io error at {path}: {source}")]
    StoreIo {
        /// Path being read or written.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A notification JSONL record could not be serialized.
    #[error("notification store serialization failed: {source}")]
    Serialize {
        /// Serialization failure.
        #[source]
        source: serde_json::Error,
    },

    /// A notification JSONL record could not be replayed.
    #[error("notification store parse error at {path}:{line}: {source}")]
    StoreParse {
        /// Path being replayed.
        path: PathBuf,
        /// One-based line number that failed to parse.
        line: usize,
        /// Parse failure.
        #[source]
        source: serde_json::Error,
    },

    /// The durable policy file could not be parsed.
    #[error("notification policy parse error at {path}: {source}")]
    PolicyParse {
        /// Path being parsed.
        path: PathBuf,
        /// Parse failure.
        #[source]
        source: serde_json::Error,
    },

    /// A requested notification does not exist.
    #[error("notification not found: {}", id.0)]
    NotFound {
        /// Missing notification id.
        id: NotificationId,
    },

    /// A requested lifecycle transition is not allowed.
    #[error("invalid notification status transition for {id:?}: {from:?} -> {to:?}")]
    InvalidTransition {
        /// Notification being updated.
        id: NotificationId,
        /// Current status.
        from: NotificationStatus,
        /// Requested status.
        to: NotificationStatus,
    },

    /// A metadata key is not safe to store.
    #[error("invalid notification metadata key `{key}`: {reason}")]
    InvalidMetadataKey {
        /// Rejected key.
        key: String,
        /// Stable explanation.
        reason: &'static str,
    },

    /// A linked session id has an invalid shape.
    #[error("invalid notification session id: {reason}")]
    InvalidSessionId {
        /// Stable explanation.
        reason: &'static str,
    },

    /// Notification policy disables the requested kind for this producer.
    #[error("notification kind {} is disabled for provider `{provider}`", kind.as_str())]
    KindDisabled {
        /// Normalized producer provider name.
        provider: String,
        /// Rejected notification kind.
        kind: NotificationKind,
    },

    /// A timestamp parameter or stored timestamp was not RFC3339.
    #[error("invalid notification timestamp `{value}`: {source}")]
    InvalidTimestamp {
        /// Invalid timestamp string.
        value: String,
        /// Parse failure.
        #[source]
        source: time::error::Parse,
    },

    /// A pagination cursor was malformed.
    #[error("invalid notification cursor `{cursor}`")]
    InvalidCursor {
        /// Rejected cursor.
        cursor: String,
    },
}

impl NotificationError {
    /// Convert the error into the daemon protocol taxonomy.
    #[must_use]
    pub fn to_protocol_error(&self) -> ProtocolError {
        match self {
            Self::StoreIo { .. }
            | Self::Serialize { .. }
            | Self::StoreParse { .. }
            | Self::PolicyParse { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "notification_store_error",
                self.to_string(),
                None,
            ),
            Self::NotFound { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "notification_not_found",
                self.to_string(),
                None,
            ),
            Self::InvalidTransition { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_notification_transition",
                self.to_string(),
                Some(
                    "use an allowed transition: unread->read, read->acknowledged, unread->acknowledged, archive a non-deleted record, or delete a non-deleted record"
                        .to_owned(),
                ),
            ),
            Self::InvalidMetadataKey { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_notification_metadata",
                self.to_string(),
                Some("remove secret-like or unsupported metadata keys".to_owned()),
            ),
            Self::InvalidSessionId { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_notification_session_id",
                self.to_string(),
                Some(
                    "send a session_id no longer than 512 bytes and without control characters"
                        .to_owned(),
                ),
            ),
            Self::KindDisabled { provider, kind } => {
                ProtocolError::notification_kind_disabled(provider, kind.as_str())
            }
            Self::InvalidTimestamp { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_notification_timestamp",
                self.to_string(),
                Some("use RFC3339 timestamps such as 2026-07-03T10:00:00Z".to_owned()),
            ),
            Self::InvalidCursor { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_notification_cursor",
                self.to_string(),
                None,
            ),
        }
    }

    /// Whether this is an invalid metadata key error.
    #[must_use]
    pub fn is_invalid_metadata_key(&self) -> bool {
        matches!(self, Self::InvalidMetadataKey { .. })
    }

    /// Whether this is an invalid session id error.
    #[must_use]
    pub fn is_invalid_session_id(&self) -> bool {
        matches!(self, Self::InvalidSessionId { .. })
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::StoreIo {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn serialize(source: serde_json::Error) -> Self {
        Self::Serialize { source }
    }

    pub(crate) fn store_parse(
        path: impl Into<PathBuf>,
        line: usize,
        source: serde_json::Error,
    ) -> Self {
        Self::StoreParse {
            path: path.into(),
            line,
            source,
        }
    }

    pub(crate) fn policy_parse(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        Self::PolicyParse {
            path: path.into(),
            source,
        }
    }
}

/// Daemon-facing notification API.
#[derive(Debug, Clone)]
pub struct NotificationService {
    inner: Arc<NotificationServiceInner>,
}

#[derive(Debug)]
struct NotificationServiceInner {
    store: NotificationStore,
    policy: RwLock<NotificationPolicy>,
    events: broadcast::Sender<Event>,
    next_id: AtomicU64,
}

impl NotificationService {
    /// Open the durable notification service under `data_dir`.
    ///
    /// The store path is `<data_dir>/notifications/notifications.jsonl`.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store cannot be opened or
    /// replayed.
    pub fn open(data_dir: &Path) -> Result<Self, NotificationError> {
        Self::open_with_policy(data_dir, default_policy())
    }

    /// Open the service with an explicit initial policy.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store cannot be opened or
    /// replayed.
    pub fn open_with_policy(
        data_dir: &Path,
        policy: NotificationPolicy,
    ) -> Result<Self, NotificationError> {
        let store = NotificationStore::open(data_dir)?;
        let policy = store.load_policy(policy)?;
        let (events, _) = broadcast::channel(NOTIFICATION_EVENT_CAPACITY);
        Ok(Self {
            inner: Arc::new(NotificationServiceInner {
                store,
                policy: RwLock::new(policy),
                events,
                next_id: AtomicU64::new(1),
            }),
        })
    }

    /// Return the durable store path.
    #[must_use]
    pub fn store_path(&self) -> &Path {
        self.inner.store.path()
    }

    /// Subscribe to notification lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Create or dedupe a notification from protocol parameters.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when validation fails or the store cannot
    /// append the resulting action.
    pub fn create(
        &self,
        params: NotificationCreateParams,
    ) -> Result<NotificationCreateResult, NotificationError> {
        self.create_with_metadata_at(params, timestamp_now())
    }

    /// Create or dedupe a notification with producer metadata.
    ///
    /// Metadata is validated against the daemon allowlist before storage.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when validation fails or the store cannot
    /// append the resulting action.
    pub fn create_with_metadata(
        &self,
        mut params: NotificationCreateParams,
        metadata: &BTreeMap<String, String>,
    ) -> Result<NotificationCreateResult, NotificationError> {
        params.metadata = metadata.clone();
        self.create_with_metadata_at(params, timestamp_now())
    }

    /// List notification records with protocol filters and cursor pagination.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when timestamp filters or cursors are
    /// invalid.
    pub fn list(
        &self,
        params: NotificationListParams,
    ) -> Result<NotificationListResult, NotificationError> {
        self.inner.store.list(params)
    }

    /// Update a notification lifecycle status.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the id is unknown or the store cannot
    /// append the update.
    pub fn update(
        &self,
        params: NotificationUpdateParams,
    ) -> Result<NotificationUpdateResult, NotificationError> {
        let now = timestamp_now();
        let outcome = self
            .inner
            .store
            .update_transition(params.id, params.status, &now)?;
        let record = match outcome {
            store::UpdateOutcome::Deleted(record) => {
                self.emit_deleted(&record.id);
                record
            }
            store::UpdateOutcome::Updated(record) => {
                self.emit_updated(&record);
                record
            }
        };
        Ok(NotificationUpdateResult { record })
    }

    /// Logically delete a notification.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store cannot append the delete.
    pub fn delete(
        &self,
        params: NotificationDeleteParams,
    ) -> Result<NotificationDeleteResult, NotificationError> {
        let id = params.id;
        let now = timestamp_now();
        let Some(record) = self.inner.store.delete_transition(&id, &now)? else {
            return Ok(NotificationDeleteResult { id, deleted: false });
        };
        self.emit_deleted(&record.id);
        Ok(NotificationDeleteResult {
            id: record.id,
            deleted: true,
        })
    }

    /// Return the current notification policy.
    #[must_use]
    pub fn policy(&self) -> NotificationPolicy {
        self.inner
            .policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the notification policy.
    pub fn set_policy(&self, policy: NotificationPolicy) -> Result<(), NotificationError> {
        let mut guard = self
            .inner
            .policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.store.write_policy(&policy)?;
        *guard = policy;
        Ok(())
    }

    /// Explicitly prune records matched by retention parameters.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when timestamp filters are invalid or the
    /// store cannot append a delete.
    pub fn prune_retention(
        &self,
        params: &NotificationRetentionParams,
    ) -> Result<NotificationRetentionResult, NotificationError> {
        let records = self.inner.store.all();
        let ids = policy::records_matching_retention(&records, params)?;
        if params.dry_run {
            return Ok(NotificationRetentionResult {
                dry_run: true,
                pruned: ids,
            });
        }
        for id in &ids {
            let _ = self.delete(NotificationDeleteParams { id: id.clone() })?;
        }
        Ok(NotificationRetentionResult {
            dry_run: false,
            pruned: ids,
        })
    }

    #[cfg(test)]
    fn create_at(
        &self,
        params: NotificationCreateParams,
        created_at: &str,
    ) -> Result<NotificationCreateResult, NotificationError> {
        self.create_with_metadata_at(params, created_at.to_owned())
    }

    fn create_with_metadata_at(
        &self,
        params: NotificationCreateParams,
        created_at: String,
    ) -> Result<NotificationCreateResult, NotificationError> {
        let mut params = normalize_params(params);
        validate_session_id(params.session_id.as_ref())?;
        let policy = self.policy();
        if !policy_enables_kind(&policy, &params.source.provider, params.kind) {
            return Err(NotificationError::KindDisabled {
                provider: params.source.provider,
                kind: params.kind,
            });
        }
        params.metadata = validate_metadata(&params.metadata)?;

        let record = NotificationRecord {
            id: self.next_id(),
            source: params.source,
            kind: params.kind,
            severity: params.severity,
            status: NotificationStatus::Unread,
            title: params.title,
            body: params.body,
            metadata: params.metadata,
            created_at,
            session_id: params.session_id,
            agent_kind: params.agent_kind,
            source_id: params.source_id,
            dedupe_key: params.dedupe_key,
            project_id: params.project_id,
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        };
        match self
            .inner
            .store
            .create_or_dedupe(record, policy.attention_dedupe_window_secs)?
        {
            store::CreateOutcome::Created(record) => {
                self.emit_created(&record);
                Ok(NotificationCreateResult {
                    created: true,
                    record,
                })
            }
            store::CreateOutcome::Existing(record) => Ok(NotificationCreateResult {
                created: false,
                record,
            }),
            store::CreateOutcome::Updated(record) => {
                self.emit_updated(&record);
                Ok(NotificationCreateResult {
                    created: false,
                    record,
                })
            }
        }
    }

    fn next_id(&self) -> NotificationId {
        let seq = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        NotificationId(format!("n-{}-{nanos}-{seq}", std::process::id()))
    }

    fn emit_created(&self, record: &NotificationRecord) {
        let payload = NotificationCreatedEvent {
            record: record.clone(),
        };
        let event = Event::new(
            event::NOTIFICATION_CREATED,
            serde_json::to_value(payload).expect("notification_created payload serializes"),
        );
        let _ = self.inner.events.send(event);
    }

    fn emit_updated(&self, record: &NotificationRecord) {
        let payload = NotificationUpdatedEvent {
            record: record.clone(),
        };
        let event = Event::new(
            event::NOTIFICATION_UPDATED,
            serde_json::to_value(payload).expect("notification_updated payload serializes"),
        );
        let _ = self.inner.events.send(event);
    }

    fn emit_deleted(&self, id: &NotificationId) {
        let payload = NotificationDeletedEvent {
            notification_id: id.clone(),
        };
        let event = Event::new(
            event::NOTIFICATION_DELETED,
            serde_json::to_value(payload).expect("notification_deleted payload serializes"),
        );
        let _ = self.inner.events.send(event);
    }
}

fn normalize_params(mut params: NotificationCreateParams) -> NotificationCreateParams {
    params.source = NotificationSource {
        provider: normalize_user_string(&params.source.provider, MAX_SOURCE_FIELD_CHARS),
        provider_event: normalize_user_string(
            &params.source.provider_event,
            MAX_SOURCE_FIELD_CHARS,
        ),
        host_local_source_id: normalize_user_string(
            &params.source.host_local_source_id,
            MAX_SOURCE_FIELD_CHARS,
        ),
    };
    params.title = normalize_user_string(&params.title, MAX_TITLE_CHARS);
    params.body = normalize_user_string(&params.body, MAX_BODY_CHARS);
    params.source_id = params
        .source_id
        .map(|value| normalize_user_string(&value, MAX_SOURCE_FIELD_CHARS));
    params.dedupe_key = params
        .dedupe_key
        .map(|value| normalize_user_string(&value, MAX_DEDUPE_KEY_CHARS));
    params.project_id = params
        .project_id
        .map(|value| normalize_user_string(&value, MAX_PROJECT_ID_CHARS));
    params
}

fn validate_session_id(session_id: Option<&protocol::SessionId>) -> Result<(), NotificationError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if session_id.0.len() > MAX_NOTIFICATION_SESSION_ID_BYTES {
        return Err(NotificationError::InvalidSessionId {
            reason: "session id exceeds maximum length",
        });
    }
    if session_id.0.chars().any(char::is_control) {
        return Err(NotificationError::InvalidSessionId {
            reason: "session id cannot contain control characters",
        });
    }
    Ok(())
}

fn normalize_user_string(value: &str, max_chars: usize) -> String {
    normalize_control_safe_whitespace(value)
        .chars()
        .take(max_chars)
        .collect()
}

fn normalize_control_safe_whitespace(value: &str) -> String {
    let control_safe = value
        .chars()
        .filter(|ch| !ch.is_control() || ch.is_whitespace())
        .collect::<String>();
    control_safe
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_metadata(
    metadata: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, NotificationError> {
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(NotificationError::InvalidMetadataKey {
            key: "<metadata>".to_owned(),
            reason: "metadata contains too many entries",
        });
    }
    let mut validated = BTreeMap::new();
    for (key, value) in metadata {
        let normalized = key.to_ascii_lowercase();
        if is_secret_metadata_key(&normalized) {
            return Err(NotificationError::InvalidMetadataKey {
                key: key.clone(),
                reason: "secret-like metadata keys are rejected",
            });
        }
        if !ALLOWED_METADATA_KEYS.contains(&normalized.as_str()) {
            return Err(NotificationError::InvalidMetadataKey {
                key: key.clone(),
                reason: "metadata key is not allowlisted",
            });
        }
        let normalized_value = normalize_control_safe_whitespace(value);
        if normalized_value.chars().count() > MAX_METADATA_VALUE_CHARS {
            return Err(NotificationError::InvalidMetadataKey {
                key: key.clone(),
                reason: "metadata value exceeds maximum length",
            });
        }
        if validated
            .insert(normalized.clone(), normalized_value)
            .is_some()
        {
            return Err(NotificationError::InvalidMetadataKey {
                key: key.clone(),
                reason: "metadata key duplicates another key case-insensitively",
            });
        }
    }
    Ok(validated)
}

fn is_secret_metadata_key(normalized: &str) -> bool {
    SECRET_METADATA_KEYS
        .iter()
        .any(|secret_key| normalized.contains(secret_key))
}

fn apply_status(record: &mut NotificationRecord, status: NotificationStatus, now: &str) {
    record.status = status;
    match status {
        NotificationStatus::Unread => {}
        NotificationStatus::Read => record.read_at = Some(now.to_owned()),
        NotificationStatus::Acknowledged => record.acked_at = Some(now.to_owned()),
        NotificationStatus::Archived => record.archived_at = Some(now.to_owned()),
        NotificationStatus::Deleted => record.deleted_at = Some(now.to_owned()),
    }
}

fn is_valid_status_transition(from: NotificationStatus, to: NotificationStatus) -> bool {
    if to == NotificationStatus::Deleted {
        return from != NotificationStatus::Deleted;
    }
    if to == NotificationStatus::Archived {
        return matches!(
            from,
            NotificationStatus::Unread
                | NotificationStatus::Read
                | NotificationStatus::Acknowledged
        );
    }
    (from == NotificationStatus::Unread && to == NotificationStatus::Read)
        || (from == NotificationStatus::Read && to == NotificationStatus::Acknowledged)
        || (from == NotificationStatus::Unread && to == NotificationStatus::Acknowledged)
}

fn same_source_namespace(left: &NotificationSource, right: &NotificationSource) -> bool {
    left.provider == right.provider && left.provider_event == right.provider_event
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePriority {
    Projector,
    User,
    Provider,
}

fn source_priority(source: &NotificationSource) -> SourcePriority {
    if source.provider.eq_ignore_ascii_case("codex")
        || source.provider.eq_ignore_ascii_case("claude")
    {
        SourcePriority::Provider
    } else if source.provider.eq_ignore_ascii_case("pohunek")
        || source.provider.eq_ignore_ascii_case("daemon")
    {
        SourcePriority::Projector
    } else {
        SourcePriority::User
    }
}

fn inside_attention_window(
    existing: &NotificationRecord,
    incoming_created_at: OffsetDateTime,
    window_secs: u64,
) -> Result<bool, NotificationError> {
    let existing_created_at = parse_timestamp(&existing.created_at)?;
    let delta = incoming_created_at - existing_created_at;
    let seconds = delta.whole_seconds().unsigned_abs();
    Ok(seconds <= window_secs)
}

pub(crate) fn parse_timestamp(value: &str) -> Result<OffsetDateTime, NotificationError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|source| NotificationError::InvalidTimestamp {
        value: value.to_owned(),
        source,
    })
}

fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::{
        ErrorClass, NotificationCreateParams, NotificationDeleteParams, NotificationKind,
        NotificationKindPolicy, NotificationListParams, NotificationRetentionParams,
        NotificationSeverity, NotificationSource, NotificationStatus, NotificationUpdateParams,
        SessionId,
    };

    use super::{
        policy::{default_policy, policy_enables_kind, DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS},
        NotificationError, NotificationService,
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_data_dir(tag: &str) -> std::path::PathBuf {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pohunek-notifications-service-{tag}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    fn projector_params(
        kind: NotificationKind,
        source_id: Option<&str>,
    ) -> NotificationCreateParams {
        params(
            "pohunek",
            "projector.agent_state",
            "projector-s-1",
            kind,
            NotificationSeverity::Warning,
            source_id,
            Some("session:s-1:attention"),
        )
    }

    fn provider_params(
        kind: NotificationKind,
        source_id: Option<&str>,
    ) -> NotificationCreateParams {
        params(
            "codex",
            "PermissionRequest",
            "codex-hook-s-1",
            kind,
            NotificationSeverity::ActionRequired,
            source_id,
            Some("session:s-1:attention"),
        )
    }

    fn params(
        provider: &str,
        provider_event: &str,
        host_local_source_id: &str,
        kind: NotificationKind,
        severity: NotificationSeverity,
        source_id: Option<&str>,
        dedupe_key: Option<&str>,
    ) -> NotificationCreateParams {
        NotificationCreateParams {
            source: NotificationSource {
                provider: provider.to_owned(),
                provider_event: provider_event.to_owned(),
                host_local_source_id: host_local_source_id.to_owned(),
            },
            kind,
            severity,
            title: "Agent needs attention".to_owned(),
            body: "The agent is waiting for the operator.".to_owned(),
            session_id: Some(SessionId("s-1".to_owned())),
            agent_kind: None,
            source_id: source_id.map(str::to_owned),
            dedupe_key: dedupe_key.map(str::to_owned),
            project_id: Some("p-1".to_owned()),
            metadata: BTreeMap::new(),
        }
    }

    fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn assert_no_controls(value: &str) {
        assert!(
            !value.chars().any(char::is_control),
            "stored notification text must not contain control characters: {value:?}"
        );
    }

    fn all_kinds_enabled_policy() -> NotificationKindPolicy {
        NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: true,
            session_finished: true,
            error: true,
            system: true,
        }
    }

    fn only_turn_completed_disabled_policy() -> NotificationKindPolicy {
        NotificationKindPolicy {
            turn_completed: false,
            ..all_kinds_enabled_policy()
        }
    }

    fn assert_notification_kind_disabled(err: &super::NotificationError) {
        let protocol = err.to_protocol_error();
        assert_eq!(protocol.class, ErrorClass::Runtime);
        assert_eq!(protocol.code, "notification_kind_disabled");
    }

    fn assert_invalid_session_id(err: &super::NotificationError) {
        let protocol = err.to_protocol_error();
        assert_eq!(protocol.class, ErrorClass::Runtime);
        assert_eq!(protocol.code, "invalid_notification_session_id");
    }

    #[test]
    fn create_is_idempotent_for_matching_source_namespace_and_source_id() {
        let service = NotificationService::open(&temp_data_dir("source-id")).expect("open service");
        let first = service
            .create(projector_params(
                NotificationKind::AgentBlocked,
                Some("same-source"),
            ))
            .expect("first create");
        let second = service
            .create(projector_params(
                NotificationKind::AgentBlocked,
                Some("same-source"),
            ))
            .expect("second create");

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.record.id, second.record.id);
    }

    #[test]
    fn create_preserves_same_source_id_in_different_source_namespaces() {
        let service =
            NotificationService::open(&temp_data_dir("source-namespace")).expect("open service");
        let first = service
            .create(params(
                "codex",
                "PermissionRequest",
                "codex-1",
                NotificationKind::ApprovalRequired,
                NotificationSeverity::ActionRequired,
                Some("same-source"),
                None,
            ))
            .expect("first create");
        let second = service
            .create(params(
                "claude",
                "Notification.permission_prompt",
                "claude-1",
                NotificationKind::ApprovalRequired,
                NotificationSeverity::ActionRequired,
                Some("same-source"),
                None,
            ))
            .expect("second create");

        assert!(first.created);
        assert!(second.created);
        assert_ne!(first.record.id, second.record.id);
    }

    #[test]
    fn create_rejects_session_id_with_control_characters() {
        let service =
            NotificationService::open(&temp_data_dir("session-id-control")).expect("open service");
        let mut params = provider_params(NotificationKind::ApprovalRequired, Some("provider-1"));
        params.session_id = Some(SessionId("s-1\ns-2".to_owned()));

        let err = service
            .create(params)
            .expect_err("control characters in session_id must fail");

        assert_invalid_session_id(&err);
        assert!(
            service
                .list(NotificationListParams::default())
                .expect("list notifications")
                .notifications
                .is_empty(),
            "invalid session_id must not append a record"
        );
    }

    #[test]
    fn create_rejects_oversized_session_id() {
        let service = NotificationService::open(&temp_data_dir("session-id-oversized"))
            .expect("open service");
        let mut params = provider_params(NotificationKind::ApprovalRequired, Some("provider-1"));
        // One byte above the existing 512-byte id-kind native session reference cap.
        params.session_id = Some(SessionId("s".repeat(513)));

        let err = service
            .create(params)
            .expect_err("oversized session_id must fail");

        assert_invalid_session_id(&err);
    }

    #[test]
    fn provider_hook_upgrades_projector_notification_inside_attention_window() {
        let service =
            NotificationService::open(&temp_data_dir("provider-upgrade")).expect("open service");
        let projector = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("projector-1")),
                "2026-07-03T10:00:00Z",
            )
            .expect("projector create");
        let provider = service
            .create_at(
                provider_params(NotificationKind::ApprovalRequired, Some("provider-1")),
                "2026-07-03T10:00:30Z",
            )
            .expect("provider create");

        assert!(!provider.created);
        assert_eq!(projector.record.id, provider.record.id);
        assert_eq!(provider.record.kind, NotificationKind::ApprovalRequired);
        assert_eq!(provider.record.source.provider, "codex");
        assert_eq!(provider.record.superseded_by, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_create_with_same_dedupe_key_produces_one_visible_record() {
        let service = Arc::new(
            NotificationService::open(&temp_data_dir("concurrent-dedupe")).expect("open service"),
        );
        let task_count = 32_usize;
        let barrier = Arc::new(Barrier::new(task_count + 1));
        let mut handles = Vec::with_capacity(task_count);
        for index in 0..task_count {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::task::spawn_blocking(move || {
                let mut params = provider_params(NotificationKind::ApprovalRequired, None);
                params.source.host_local_source_id = format!("hook-{index}");
                params.dedupe_key = Some("session:s-1:concurrent-attention".to_owned());
                barrier.wait();
                service.create(params)
            }));
        }
        barrier.wait();

        let mut results = Vec::with_capacity(task_count);
        for handle in handles {
            results.push(
                handle
                    .await
                    .expect("create task completed")
                    .expect("create succeeds"),
            );
        }

        let created_count = results.iter().filter(|result| result.created).count();
        assert_eq!(
            created_count, 1,
            "exactly one concurrent create may append the visible record"
        );
        assert_eq!(
            results.iter().filter(|result| !result.created).count(),
            task_count - 1
        );

        let listed = service
            .list(NotificationListParams::default())
            .expect("list notifications");
        assert_eq!(listed.notifications.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_delete_wins_over_stale_read_updates_after_replay() {
        const UPDATE_TASKS: usize = 64;
        const ATTEMPTS: usize = 16;

        for attempt in 0..ATTEMPTS {
            let data_dir = temp_data_dir(&format!("delete-read-race-{attempt}"));
            let service = Arc::new(NotificationService::open(&data_dir).expect("open service"));
            let created = service
                .create(provider_params(
                    NotificationKind::ApprovalRequired,
                    Some(&format!("delete-read-race-{attempt}")),
                ))
                .expect("create notification");
            let id = created.record.id.clone();
            let task_count = UPDATE_TASKS + 1;
            let barrier = Arc::new(Barrier::new(task_count + 1));
            let mut update_handles = Vec::with_capacity(UPDATE_TASKS);

            for _ in 0..UPDATE_TASKS {
                let service = Arc::clone(&service);
                let barrier = Arc::clone(&barrier);
                let id = id.clone();
                update_handles.push(tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    service.update(NotificationUpdateParams {
                        id,
                        status: NotificationStatus::Read,
                    })
                }));
            }

            let delete_handle = {
                let service = Arc::clone(&service);
                let barrier = Arc::clone(&barrier);
                let id = id.clone();
                tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    service.delete(NotificationDeleteParams { id })
                })
            };
            barrier.wait();

            for handle in update_handles {
                match handle.await.expect("update task completed") {
                    Ok(result) => assert_eq!(result.record.status, NotificationStatus::Read),
                    Err(NotificationError::InvalidTransition { from, to, .. }) => {
                        assert!(
                            matches!(from, NotificationStatus::Read | NotificationStatus::Deleted),
                            "unexpected read-race source status: {from:?}"
                        );
                        assert_eq!(to, NotificationStatus::Read);
                    }
                    Err(err) => panic!("unexpected update error: {err:?}"),
                }
            }
            let delete_result = delete_handle
                .await
                .expect("delete task completed")
                .expect("delete succeeds");
            assert!(delete_result.deleted, "delete must win attempt {attempt}");

            let reopened = NotificationService::open(&data_dir).expect("reopen service");
            let deleted = reopened
                .list(NotificationListParams {
                    status: Some(NotificationStatus::Deleted),
                    ..NotificationListParams::default()
                })
                .expect("list deleted notifications");
            assert_eq!(
                deleted.notifications.len(),
                1,
                "delete must be the terminal replayed state on attempt {attempt}"
            );
            assert_eq!(deleted.notifications[0].id, id);
            assert_eq!(deleted.notifications[0].status, NotificationStatus::Deleted);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_delete_and_ack_yield_terminal_delete_or_typed_update_error() {
        const UPDATE_TASKS: usize = 64;
        const ATTEMPTS: usize = 16;

        for attempt in 0..ATTEMPTS {
            let data_dir = temp_data_dir(&format!("delete-ack-race-{attempt}"));
            let service = Arc::new(NotificationService::open(&data_dir).expect("open service"));
            let created = service
                .create(provider_params(
                    NotificationKind::ApprovalRequired,
                    Some(&format!("delete-ack-race-{attempt}")),
                ))
                .expect("create notification");
            let id = created.record.id.clone();
            service
                .update(NotificationUpdateParams {
                    id: id.clone(),
                    status: NotificationStatus::Read,
                })
                .expect("mark notification read before ack race");

            let task_count = UPDATE_TASKS + 1;
            let barrier = Arc::new(Barrier::new(task_count + 1));
            let mut update_handles = Vec::with_capacity(UPDATE_TASKS);
            for _ in 0..UPDATE_TASKS {
                let service = Arc::clone(&service);
                let barrier = Arc::clone(&barrier);
                let id = id.clone();
                update_handles.push(tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    service.update(NotificationUpdateParams {
                        id,
                        status: NotificationStatus::Acknowledged,
                    })
                }));
            }

            let delete_handle = {
                let service = Arc::clone(&service);
                let barrier = Arc::clone(&barrier);
                let id = id.clone();
                tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    service.delete(NotificationDeleteParams { id })
                })
            };
            barrier.wait();

            for handle in update_handles {
                match handle.await.expect("update task completed") {
                    Ok(result) => {
                        assert_eq!(result.record.status, NotificationStatus::Acknowledged);
                    }
                    Err(NotificationError::InvalidTransition { from, to, .. }) => {
                        assert!(
                            matches!(
                                from,
                                NotificationStatus::Acknowledged | NotificationStatus::Deleted
                            ),
                            "unexpected ack-race source status: {from:?}"
                        );
                        assert_eq!(to, NotificationStatus::Acknowledged);
                    }
                    Err(err) => panic!("unexpected update error: {err:?}"),
                }
            }
            let delete_result = delete_handle
                .await
                .expect("delete task completed")
                .expect("delete succeeds");
            assert!(delete_result.deleted, "delete must win attempt {attempt}");

            let reopened = NotificationService::open(&data_dir).expect("reopen service");
            let deleted = reopened
                .list(NotificationListParams {
                    status: Some(NotificationStatus::Deleted),
                    ..NotificationListParams::default()
                })
                .expect("list deleted notifications");
            assert_eq!(
                deleted.notifications.len(),
                1,
                "acked update must not resurrect deleted notification on attempt {attempt}"
            );
            assert_eq!(deleted.notifications[0].id, id);
            assert_eq!(deleted.notifications[0].status, NotificationStatus::Deleted);
        }
    }

    #[test]
    fn projector_create_is_suppressed_by_existing_provider_inside_attention_window() {
        let service = NotificationService::open(&temp_data_dir("projector-suppressed"))
            .expect("open service");
        let provider = service
            .create_at(
                provider_params(NotificationKind::ApprovalRequired, Some("provider-1")),
                "2026-07-03T10:00:00Z",
            )
            .expect("provider create");
        let projector = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("projector-1")),
                "2026-07-03T10:00:30Z",
            )
            .expect("projector create");

        assert!(provider.created);
        assert!(!projector.created);
        assert_eq!(provider.record.id, projector.record.id);
        assert_eq!(projector.record.kind, NotificationKind::ApprovalRequired);
    }

    #[test]
    fn dedupe_key_outside_attention_window_creates_separate_notification() {
        let service =
            NotificationService::open(&temp_data_dir("outside-window")).expect("open service");
        let first = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("projector-1")),
                "2026-07-03T10:00:00Z",
            )
            .expect("first create");
        let second = service
            .create_at(
                provider_params(NotificationKind::ApprovalRequired, Some("provider-1")),
                "2026-07-03T11:00:00Z",
            )
            .expect("second create");

        assert!(first.created);
        assert!(second.created);
        assert_ne!(first.record.id, second.record.id);
    }

    #[test]
    fn metadata_rejects_secret_like_keys_case_insensitively() {
        let service = NotificationService::open(&temp_data_dir("metadata")).expect("open service");
        for key in [
            "token",
            "secret",
            "password",
            "api_key",
            "access_token",
            "authorization",
            "cookie",
        ] {
            let mut params = projector_params(NotificationKind::AgentBlocked, None);
            params.metadata = BTreeMap::from([(key.to_ascii_uppercase(), "redacted".to_owned())]);
            let err = service
                .create(params)
                .expect_err("secret-like key must fail");
            assert!(err.is_invalid_metadata_key());
        }
    }

    #[test]
    fn metadata_rejects_unallowlisted_keys() {
        let service =
            NotificationService::open(&temp_data_dir("metadata-disallowed")).expect("open service");
        let mut params = projector_params(NotificationKind::AgentBlocked, None);
        params.metadata = metadata(&[("unsupported", "value")]);

        let err = service
            .create(params)
            .expect_err("unallowlisted key must fail");

        assert!(err.is_invalid_metadata_key());
    }

    #[test]
    fn metadata_enforces_entry_and_value_bounds() {
        let service =
            NotificationService::open(&temp_data_dir("metadata-bounds")).expect("open service");
        let mut too_many = projector_params(NotificationKind::AgentBlocked, None);
        too_many.metadata = metadata(&[
            ("action_url", "https://example.invalid/action"),
            ("detail_url", "https://example.invalid/detail"),
            ("provider", "codex"),
            ("provider_event", "PermissionRequest"),
            ("reason", "approval"),
            ("summary", "summary"),
            ("hook_event_id", "hook-1"),
            ("matcher", "permission_prompt"),
            ("tool_name", "shell"),
        ]);
        assert!(
            too_many.metadata.len() > super::MAX_METADATA_ENTRIES,
            "test must exceed metadata entry bound"
        );
        let err = service
            .create(too_many)
            .expect_err("too many metadata entries must fail");
        assert!(err.is_invalid_metadata_key());

        let mut oversized = projector_params(NotificationKind::AgentBlocked, None);
        oversized.metadata = BTreeMap::from([(
            "summary".to_owned(),
            "x".repeat(super::MAX_METADATA_VALUE_CHARS + 1),
        )]);
        let err = service
            .create(oversized)
            .expect_err("oversized metadata value must fail");
        assert!(err.is_invalid_metadata_key());
    }

    #[test]
    fn metadata_is_sanitized_persisted_and_replayed() {
        let data_dir = temp_data_dir("metadata-replay");
        let service = NotificationService::open(&data_dir).expect("open service");
        let mut params = projector_params(NotificationKind::AgentBlocked, None);
        params.metadata = metadata(&[
            ("provider", "codex\u{1b}"),
            ("reason", "approval\u{7}\nneeded"),
        ]);

        let created = service.create(params).expect("create");

        assert_eq!(
            created.record.metadata,
            metadata(&[("provider", "codex"), ("reason", "approval needed")])
        );

        let reopened = NotificationService::open(&data_dir).expect("reopen service");
        let listed = reopened
            .list(NotificationListParams::default())
            .expect("list notifications");
        assert_eq!(listed.notifications.len(), 1);
        assert_eq!(listed.notifications[0].metadata, created.record.metadata);
    }

    #[test]
    fn title_and_body_are_normalized_before_storage() {
        let service = NotificationService::open(&temp_data_dir("normalize")).expect("open service");
        let mut params = projector_params(NotificationKind::AgentBlocked, None);
        params.title = "  Agent\nneeds\tattention  ".to_owned();
        params.body = "  Body line  ".repeat(10_000);

        let created = service.create(params).expect("create");

        assert_eq!(created.record.title, "Agent needs attention");
        assert!(created.record.body.len() <= super::MAX_BODY_CHARS);
    }

    #[test]
    fn user_controlled_strings_strip_control_characters_before_storage() {
        let service =
            NotificationService::open(&temp_data_dir("strip-controls")).expect("open service");
        let mut params = projector_params(NotificationKind::AgentBlocked, None);
        params.source.provider = "cod\u{1b}ex".to_owned();
        params.source.provider_event = "Permission\u{7}Request".to_owned();
        params.source.host_local_source_id = "hook\u{1b}[0m-1".to_owned();
        params.title = "\u{1b}[31m Agent\nneeds\u{7} attention".to_owned();
        params.body = "Body\u{1b}[0m\nline\u{7}".to_owned();
        params.source_id = Some("source\u{1b}-1".to_owned());
        params.dedupe_key = Some("dedupe\u{7}-1".to_owned());
        params.project_id = Some("project\u{1b}-1".to_owned());
        params.metadata = metadata(&[("summary", "Summary\u{1b}[0m\nline\u{7}")]);

        let created = service.create(params).expect("create");

        assert_no_controls(&created.record.source.provider);
        assert_no_controls(&created.record.source.provider_event);
        assert_no_controls(&created.record.source.host_local_source_id);
        assert_no_controls(&created.record.title);
        assert_no_controls(&created.record.body);
        assert_no_controls(created.record.source_id.as_deref().expect("source id"));
        assert_no_controls(created.record.dedupe_key.as_deref().expect("dedupe key"));
        assert_no_controls(created.record.project_id.as_deref().expect("project id"));
        assert_no_controls(
            created
                .record
                .metadata
                .get("summary")
                .expect("summary metadata"),
        );
    }

    #[test]
    fn default_policy_enables_attention_and_error_but_not_turn_completed() {
        let policy = default_policy();

        assert!(policy_enables_kind(
            &policy,
            "codex",
            NotificationKind::AgentBlocked
        ));
        assert!(policy_enables_kind(
            &policy,
            "claude",
            NotificationKind::ApprovalRequired
        ));
        assert!(policy_enables_kind(
            &policy,
            "codex",
            NotificationKind::Error
        ));
        assert!(!policy_enables_kind(
            &policy,
            "codex",
            NotificationKind::TurnCompleted
        ));
    }

    #[test]
    fn default_policy_contains_provider_specific_codex_and_claude_overrides() {
        let policy = default_policy();

        assert!(policy.codex.is_some());
        assert!(policy.claude.is_some());
        assert_eq!(
            policy.attention_dedupe_window_secs,
            DEFAULT_ATTENTION_DEDUPE_WINDOW_SECS
        );
    }

    #[test]
    fn create_rejects_policy_disabled_kind_without_appending() {
        let service =
            NotificationService::open(&temp_data_dir("policy-disabled")).expect("open service");
        let params = provider_params(NotificationKind::TurnCompleted, Some("turn-1"));

        let err = service
            .create(params)
            .expect_err("turn_completed is disabled by default");

        assert_notification_kind_disabled(&err);
        assert!(
            service
                .list(NotificationListParams::default())
                .expect("list notifications")
                .notifications
                .is_empty(),
            "disabled kind must not append a record"
        );
    }

    #[test]
    fn create_allows_policy_enabled_kind() {
        let service =
            NotificationService::open(&temp_data_dir("policy-enabled")).expect("open service");

        let result = service
            .create(provider_params(
                NotificationKind::ApprovalRequired,
                Some("provider-1"),
            ))
            .expect("approval_required is enabled by default");

        assert!(result.created);
        assert_eq!(result.record.kind, NotificationKind::ApprovalRequired);
    }

    #[test]
    fn create_respects_provider_specific_policy_override() {
        let service = NotificationService::open(&temp_data_dir("policy-provider-override"))
            .expect("open service");
        let mut policy = service.policy();
        policy.enabled = only_turn_completed_disabled_policy();
        policy.codex = Some(all_kinds_enabled_policy());
        policy.claude = None;
        service.set_policy(policy).expect("set policy");

        let codex_result = service
            .create(provider_params(
                NotificationKind::TurnCompleted,
                Some("codex-turn"),
            ))
            .expect("codex override enables turn_completed");
        let claude_err = service
            .create(params(
                "claude",
                "Stop",
                "claude-hook-s-1",
                NotificationKind::TurnCompleted,
                NotificationSeverity::Info,
                Some("claude-turn"),
                None,
            ))
            .expect_err("default policy still disables claude turn_completed");

        assert!(codex_result.created);
        assert_notification_kind_disabled(&claude_err);
    }

    #[test]
    fn policy_set_takes_effect_for_subsequent_creates() {
        let service =
            NotificationService::open(&temp_data_dir("policy-set-live")).expect("open service");
        let mut disabled = service.policy();
        disabled.enabled = only_turn_completed_disabled_policy();
        disabled.codex = None;
        disabled.claude = None;
        service.set_policy(disabled).expect("set disabled policy");

        let disabled_err = service
            .create(provider_params(
                NotificationKind::TurnCompleted,
                Some("turn-disabled"),
            ))
            .expect_err("turn_completed disabled");
        assert_notification_kind_disabled(&disabled_err);

        let mut enabled = service.policy();
        enabled.enabled = all_kinds_enabled_policy();
        service.set_policy(enabled).expect("set enabled policy");
        let created = service
            .create(provider_params(
                NotificationKind::TurnCompleted,
                Some("turn-enabled"),
            ))
            .expect("turn_completed enabled");
        assert!(created.created);

        let mut disabled_again = service.policy();
        disabled_again.enabled = only_turn_completed_disabled_policy();
        service
            .set_policy(disabled_again)
            .expect("set disabled policy again");
        let disabled_again_err = service
            .create(provider_params(
                NotificationKind::TurnCompleted,
                Some("turn-disabled-again"),
            ))
            .expect_err("turn_completed disabled again");
        assert_notification_kind_disabled(&disabled_again_err);
    }

    #[test]
    fn configured_attention_window_controls_cross_producer_dedupe() {
        let service =
            NotificationService::open(&temp_data_dir("configured-window")).expect("open service");
        let mut policy = service.policy();
        policy.attention_dedupe_window_secs = 10;
        service.set_policy(policy).expect("set policy");

        let first = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("projector-1")),
                "2026-07-03T10:00:00Z",
            )
            .expect("first create");
        let second = service
            .create_at(
                provider_params(NotificationKind::ApprovalRequired, Some("provider-1")),
                "2026-07-03T10:00:11Z",
            )
            .expect("second create");

        assert!(first.created);
        assert!(second.created);
        assert_ne!(first.record.id, second.record.id);
    }

    #[test]
    fn policy_changes_are_persisted_across_reopen() {
        let data_dir = temp_data_dir("policy-reopen");
        let service = NotificationService::open(&data_dir).expect("open service");
        let policy = protocol::NotificationPolicy {
            attention_dedupe_window_secs: 42,
            enabled: NotificationKindPolicy {
                agent_blocked: true,
                approval_required: true,
                turn_completed: true,
                session_finished: true,
                error: true,
                system: true,
            },
            codex: None,
            claude: None,
        };

        service.set_policy(policy.clone()).expect("set policy");

        let reopened = NotificationService::open(&data_dir).expect("reopen service");
        assert_eq!(reopened.policy(), policy);
    }

    #[test]
    fn prune_retention_dry_run_skips_deleted_when_status_is_unfiltered() {
        let service =
            NotificationService::open(&temp_data_dir("retention-dry-run")).expect("open service");
        let old_archived = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("old-archived")),
                "2026-07-01T00:00:00Z",
            )
            .expect("create old archived");
        service
            .update(NotificationUpdateParams {
                id: old_archived.record.id.clone(),
                status: NotificationStatus::Archived,
            })
            .expect("archive old record");
        let old_deleted = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("old-deleted")),
                "2026-07-01T01:00:00Z",
            )
            .expect("create old deleted");
        service
            .delete(NotificationDeleteParams {
                id: old_deleted.record.id.clone(),
            })
            .expect("delete old record");

        let result = service
            .prune_retention(&NotificationRetentionParams {
                dry_run: true,
                status: None,
                before: Some("2026-07-03T00:00:00Z".to_owned()),
                limit: None,
            })
            .expect("dry-run prune");

        assert_eq!(result.pruned, vec![old_archived.record.id]);
        assert!(result.dry_run);
    }

    #[test]
    fn prune_retention_apply_deletes_matched_records() {
        let service =
            NotificationService::open(&temp_data_dir("retention-apply")).expect("open service");
        let old_archived = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("old-archived")),
                "2026-07-01T00:00:00Z",
            )
            .expect("create old archived");
        service
            .update(NotificationUpdateParams {
                id: old_archived.record.id.clone(),
                status: NotificationStatus::Archived,
            })
            .expect("archive old record");
        let new_archived = service
            .create_at(
                projector_params(NotificationKind::AgentBlocked, Some("new-archived")),
                "2026-07-04T00:00:00Z",
            )
            .expect("create new archived");
        service
            .update(NotificationUpdateParams {
                id: new_archived.record.id.clone(),
                status: NotificationStatus::Archived,
            })
            .expect("archive new record");

        let result = service
            .prune_retention(&NotificationRetentionParams {
                dry_run: false,
                status: Some(NotificationStatus::Archived),
                before: Some("2026-07-03T00:00:00Z".to_owned()),
                limit: None,
            })
            .expect("apply prune");

        assert_eq!(result.pruned, vec![old_archived.record.id.clone()]);
        assert!(!result.dry_run);
        let deleted = service
            .list(NotificationListParams {
                status: Some(NotificationStatus::Deleted),
                ..NotificationListParams::default()
            })
            .expect("list deleted");
        assert_eq!(deleted.notifications[0].id, old_archived.record.id);
    }
}
