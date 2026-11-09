//! Durable daemon notification service.

mod coordinator;
mod notice_sink;
mod projector;
mod retention;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use pohunek_service_config::{NotificationSettings, DEFAULT_NOTIFICATION_SETTINGS};
use protocol::{
    event, ErrorClass, Event, NotificationCreateParams, NotificationCreateResult,
    NotificationCreatedEvent, NotificationDeletedEvent, NotificationId, NotificationKind,
    NotificationListParams, NotificationListResult, NotificationRecord, NotificationSource,
    NotificationStatus, NotificationUpdatedEvent, ProtocolError,
};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::broadcast;

use crate::session::TurnId;

#[doc(inline)]
pub use coordinator::AttentionCoordinator;
#[cfg(test)]
pub(crate) use coordinator::DeferProbe;
pub(crate) use coordinator::ResolveAsOf;
#[doc(inline)]
pub use notice_sink::ApprovalNoticeSink;
#[doc(inline)]
pub use projector::{attention_dedupe_key, spawn_projector, turn_dedupe_key};
#[doc(inline)]
pub use retention::spawn_notification_retention;
#[doc(inline)]
pub use store::NOTIFICATIONS_SUBDIR;

pub(crate) use store::NotificationStore;

/// A session notification prepared for the debounce coordinator.
///
/// `inbox` says whether the record is an attention record the store takes.
/// `signal` says whether the record also stands for a session turn event for
/// `session.wait_turn`, which is recorded regardless of the inbox; only the
/// projector and admitted provider hook claims for a session the registry holds
/// set it.
#[derive(Debug, Clone)]
pub(crate) struct DeferredNotification {
    /// Validated record with its id minted.
    pub(crate) record: NotificationRecord,
    /// Whether the record is kept in the inbox: an attention kind and a store
    /// that is in use.
    pub(crate) inbox: bool,
    /// Why the store cannot take the record; the turn signal is still delivered.
    pub(crate) unavailable: Option<String>,
    /// Whether the record comes from a producer trusted to report turn events.
    pub(crate) signal: bool,
    /// The turn the record reports, when it reports one the daemon runs.
    /// Producers meet on it; only the daemon says which it is.
    pub(crate) turn: Option<TurnId>,
    /// When the record was minted, on a monotonic clock.
    pub(crate) minted: Instant,
}

impl DeferredNotification {
    /// Attaches the record to the turn it reports; any kind but
    /// `turn_completed` is returned unchanged.
    #[must_use]
    pub(crate) fn for_turn(mut self, turn: TurnId) -> Self {
        if self.record.kind == NotificationKind::TurnCompleted {
            self.turn = Some(turn);
        }
        self
    }

    /// The error an immediate create of an attention record reports when the
    /// store cannot take it.
    pub(crate) fn store_refusal(&self) -> Option<NotificationError> {
        if !is_attention_kind(self.record.kind) {
            return None;
        }
        self.unavailable
            .as_ref()
            .map(|reason| NotificationError::unavailable(reason.clone()))
    }
}

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

/// Prefix of the shared session attention dedupe key (`attention:<session_id>`).
///
/// Kept in sync with [`projector::attention_dedupe_key`], which builds keys with
/// this prefix. Routing a create to the debounce coordinator matches on it.
pub(crate) const ATTENTION_DEDUPE_KEY_PREFIX: &str = "attention";

/// Prefix of the shared session turn-completion dedupe key (`turn:<session_id>`).
///
/// Turn keys name the in-memory turn signal of a session. Like attention keys,
/// they require a session suffix.
pub(crate) const TURN_DEDUPE_KEY_PREFIX: &str = "turn";

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
    "cause",
    "oom_killer",
    "memory_peak_bytes",
    "loss_cause",
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

    /// This build does not use the notification store, so notifications are
    /// unavailable until the store is readable again.
    #[error("notifications are unavailable: {reason}")]
    Unavailable {
        /// Why the store is not used.
        reason: String,
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

    /// A reserved dedupe key does not match its session-scoped contract.
    #[error("invalid notification dedupe key `{key}`: {reason}")]
    InvalidDedupeKey {
        /// Rejected key.
        key: String,
        /// Stable explanation.
        reason: &'static str,
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

    /// A daemon-owned notification timestamp could not be formatted.
    #[error("notification timestamp formatting failed: {source}")]
    TimestampFormat {
        /// Formatting failure.
        #[source]
        source: time::error::Format,
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
            | Self::TimestampFormat { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "notification_store_error",
                self.to_string(),
                None,
            ),
            Self::Unavailable { .. } => ProtocolError::new(
                ErrorClass::Daemon,
                "notifications_unavailable",
                self.to_string(),
                Some(
                    "upgrade pohunek to a build that reads this notification store, or move the notifications directory aside to start a new one; the existing files were not changed"
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
            Self::InvalidDedupeKey { .. } => ProtocolError::new(
                ErrorClass::Runtime,
                "invalid_notification_dedupe_key",
                self.to_string(),
                Some(
                    "send attention:<session_id> only for attention notifications and turn:<session_id> only for turn reports, with a matching session_id"
                        .to_owned(),
                ),
            ),
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

    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

/// Outcome of one retention sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionOutcome {
    /// Records removed from the store.
    pub removed: usize,
    /// Removed records announced with `notification_deleted`.
    pub announced: usize,
}

/// Daemon-facing notification API.
#[derive(Debug, Clone)]
pub struct NotificationService {
    inner: Arc<NotificationServiceInner>,
}

#[derive(Debug)]
struct NotificationServiceInner {
    store: NotificationStore,
    settings: NotificationSettings,
    events: broadcast::Sender<Event>,
    next_id: AtomicU64,
}

impl NotificationService {
    /// Open the durable notification service under `data_dir` with the
    /// documented default settings.
    ///
    /// The store path is `<data_dir>/notifications/notifications.jsonl`.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store cannot be opened or
    /// replayed.
    pub fn open(data_dir: &Path) -> Result<Self, NotificationError> {
        Self::open_with_settings(data_dir, DEFAULT_NOTIFICATION_SETTINGS)
    }

    /// Open the service with explicit settings.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store cannot be opened or
    /// replayed.
    pub fn open_with_settings(
        data_dir: &Path,
        settings: NotificationSettings,
    ) -> Result<Self, NotificationError> {
        let store = NotificationStore::open(data_dir)?;
        let (events, _) = broadcast::channel(NOTIFICATION_EVENT_CAPACITY);
        Ok(Self {
            inner: Arc::new(NotificationServiceInner {
                store,
                settings,
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

    /// The source ids of `provider`'s stored records that start with
    /// `prefix`, with the prefix removed, each with its record's creation
    /// timestamp.
    #[must_use]
    pub(crate) fn source_id_suffixes(&self, provider: &str, prefix: &str) -> Vec<(String, String)> {
        self.inner.store.source_id_suffixes(provider, prefix)
    }

    /// Whether an unread `agent_blocked` or `approval_required` record carries
    /// `dedupe_key`.
    #[must_use]
    pub(crate) fn has_unread_attention(&self, dedupe_key: &str) -> bool {
        self.inner.store.has_unread_attention(dedupe_key)
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

    /// Create or dedupe a notification the daemon admitted as a hook claim of
    /// its session's runtime; the stored record carries the marker that ranks
    /// it as a provider report.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when validation fails or the store cannot
    /// append the resulting action.
    pub(crate) fn create_hook_claim(
        &self,
        params: NotificationCreateParams,
    ) -> Result<NotificationCreateResult, NotificationError> {
        let mut record = self.build_record(params, timestamp_now())?;
        record.hook_admitted = true;
        self.commit_record(record, Instant::now())
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

    /// Whether `session_id` has an `agent_blocked` or `approval_required`
    /// notification that is still `unread` or `read`.
    ///
    /// Records the debounce coordinator still holds are not stored yet and do not
    /// count.
    #[must_use]
    pub fn has_active_attention(&self, session_id: &protocol::SessionId) -> bool {
        // Without the store the attention records are unknown, so a session is
        // treated as awaiting the owner rather than stopped as idle.
        self.inner.store.ensure_available().is_err()
            || self.inner.store.has_active_attention(session_id)
    }

    /// Acknowledge active session notifications for `dedupe_key`.
    ///
    /// Called when a session resumes active work so `agent_blocked` and
    /// `approval_required` notifications stop lingering as unread. Emits `notification_updated` for each
    /// acknowledged record and returns them; a dedupe key with no active matching
    /// notification is a no-op.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store append fails.
    pub(crate) fn resolve_session_notifications(
        &self,
        dedupe_key: &str,
        as_of: Option<&ResolveAsOf>,
    ) -> Result<Vec<NotificationRecord>, NotificationError> {
        let now = timestamp_now();
        let resolved = self
            .inner
            .store
            .resolve_session_notifications(dedupe_key, &now, as_of)?;
        for record in &resolved {
            self.emit_updated(record);
        }
        Ok(resolved)
    }

    /// Fails with the typed unavailable error while this build does not use
    /// the notification store.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError::Unavailable`] when the store is not used.
    pub fn ensure_available(&self) -> Result<(), NotificationError> {
        self.inner.store.ensure_available()
    }

    /// The debounce, dedupe and retention settings of this service.
    #[must_use]
    pub fn settings(&self) -> NotificationSettings {
        self.inner.settings
    }

    /// Run one retention sweep at `now`.
    ///
    /// Removes the records of kinds the inbox does not hold, deleted records,
    /// and resolved attention records older than the configured lifetime, then
    /// rewrites the log. A removed attention record emits
    /// `notification_deleted`; open attention records are kept.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when a timestamp cannot be read or the
    /// rewrite fails; the previous log is intact then.
    pub fn run_retention_at(
        &self,
        now: OffsetDateTime,
    ) -> Result<RetentionOutcome, NotificationError> {
        let removed = self
            .inner
            .store
            .sweep(now, self.inner.settings.record_ttl_secs)?;
        let mut deleted = 0;
        for record in &removed {
            if is_attention_kind(record.kind) && record.status != NotificationStatus::Deleted {
                self.emit_deleted(&record.id);
                deleted += 1;
            }
        }
        Ok(RetentionOutcome {
            removed: removed.len(),
            announced: deleted,
        })
    }

    /// Build a normalized, validated notification record without persisting it.
    ///
    /// Mints the id now so the `notification.create` response can return it, but
    /// the caller (the debounce coordinator) holds the record pending and commits
    /// it later via [`Self::commit_deferred`]. Performs no store I/O. A kind the
    /// inbox does not hold is not an error here: the result says the inbox drops
    /// it. The result is not a turn signal until the caller marks it as one.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when validation fails, exactly as an
    /// immediate create would.
    pub(crate) fn prepare_deferred(
        &self,
        params: NotificationCreateParams,
    ) -> Result<DeferredNotification, NotificationError> {
        self.prepare_record(params, timestamp_now())
    }

    /// Commit a previously prepared deferred record and broadcast its event.
    ///
    /// This is the debounce coordinator's flush path: the record is run through
    /// the store's `create_or_dedupe` (the final dedupe authority) and, when it
    /// becomes visible, a `notification_created`/`notification_updated` event is
    /// emitted exactly as an immediate create would.
    ///
    /// # Errors
    ///
    /// Returns a [`NotificationError`] when the store append fails.
    pub(crate) fn commit_deferred(
        &self,
        record: NotificationRecord,
        minted: Instant,
    ) -> Result<NotificationCreateResult, NotificationError> {
        self.commit_record(record, minted)
    }

    fn create_with_metadata_at(
        &self,
        params: NotificationCreateParams,
        created_at: String,
    ) -> Result<NotificationCreateResult, NotificationError> {
        let record = self.build_record(params, created_at)?;
        self.commit_record(record, Instant::now())
    }

    fn build_record(
        &self,
        params: NotificationCreateParams,
        created_at: String,
    ) -> Result<NotificationRecord, NotificationError> {
        let prepared = self.prepare_record(params, created_at)?;
        match prepared.store_refusal() {
            Some(refusal) => Err(refusal),
            None => Ok(prepared.record),
        }
    }

    /// Validates and assembles a record, reporting whether the inbox keeps it.
    ///
    /// Session id and reserved dedupe key validation fail for every record. A
    /// record the inbox does not hold never reaches the store, so its metadata
    /// is validated only for a kept record and is otherwise left empty.
    fn prepare_record(
        &self,
        params: NotificationCreateParams,
        created_at: String,
    ) -> Result<DeferredNotification, NotificationError> {
        let unavailable = match self.inner.store.ensure_available() {
            Ok(()) => None,
            Err(NotificationError::Unavailable { reason }) => Some(reason),
            Err(other) => return Err(other),
        };
        let mut params = normalize_params(params);
        validate_session_id(params.session_id.as_ref())?;
        validate_reserved_dedupe_key(
            params.session_id.as_ref(),
            params.kind,
            params.dedupe_key.as_deref(),
        )?;
        let inbox = unavailable.is_none() && is_attention_kind(params.kind);
        params.metadata = if inbox {
            validate_metadata(&params.metadata)?
        } else {
            BTreeMap::new()
        };

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
            turn_started_at: None,
            turn_duration_ms: None,
            session_metadata: BTreeMap::new(),
            hook_admitted: false,
        };
        Ok(DeferredNotification {
            record,
            inbox,
            unavailable,
            signal: false,
            turn: None,
            minted: Instant::now(),
        })
    }

    fn commit_record(
        &self,
        record: NotificationRecord,
        minted: Instant,
    ) -> Result<NotificationCreateResult, NotificationError> {
        if !is_attention_kind(record.kind) {
            // Only attention kinds are ever stored.
            return Ok(NotificationCreateResult {
                created: false,
                record,
            });
        }
        let window = self.inner.settings.attention_dedupe_window_secs;
        match self.inner.store.create_or_dedupe(record, window, minted)? {
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
        let event = crate::events::event(
            event::NOTIFICATION_CREATED,
            serde_json::to_value(payload).expect("notification_created payload serializes"),
        );
        let _ = self.inner.events.send(event);
    }

    fn emit_updated(&self, record: &NotificationRecord) {
        let payload = NotificationUpdatedEvent {
            record: record.clone(),
        };
        let event = crate::events::event(
            event::NOTIFICATION_UPDATED,
            serde_json::to_value(payload).expect("notification_updated payload serializes"),
        );
        let _ = self.inner.events.send(event);
    }

    fn emit_deleted(&self, id: &NotificationId) {
        let payload = NotificationDeletedEvent {
            notification_id: id.clone(),
        };
        let event = crate::events::event(
            event::NOTIFICATION_DELETED,
            serde_json::to_value(payload).expect("notification_deleted payload serializes"),
        );
        let _ = self.inner.events.send(event);
    }
}

fn normalize_params(mut params: NotificationCreateParams) -> NotificationCreateParams {
    params.source = NotificationSource {
        provider: stored_provider(&params.source.provider),
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

fn validate_reserved_dedupe_key(
    session_id: Option<&protocol::SessionId>,
    kind: NotificationKind,
    dedupe_key: Option<&str>,
) -> Result<(), NotificationError> {
    let Some(dedupe_key) = dedupe_key else {
        return Ok(());
    };
    let Some((prefix, suffix)) = reserved_dedupe_session_suffix(dedupe_key) else {
        return Ok(());
    };
    match prefix {
        ATTENTION_DEDUPE_KEY_PREFIX if !is_attention_kind(kind) && !is_turn_report_kind(kind) => {
            return Err(NotificationError::InvalidDedupeKey {
                key: dedupe_key.to_owned(),
                reason: "attention dedupe key requires an attention notification kind",
            });
        }
        TURN_DEDUPE_KEY_PREFIX if !is_turn_report_kind(kind) => {
            return Err(NotificationError::InvalidDedupeKey {
                key: dedupe_key.to_owned(),
                reason: "turn dedupe key requires a turn report kind",
            });
        }
        _ => {}
    }
    let Some(session_id) = session_id else {
        return Err(NotificationError::InvalidDedupeKey {
            key: dedupe_key.to_owned(),
            reason: "reserved dedupe key requires matching session_id",
        });
    };
    if suffix != session_id.0 {
        return Err(NotificationError::InvalidDedupeKey {
            key: dedupe_key.to_owned(),
            reason: "reserved dedupe key session suffix must match session_id",
        });
    }
    Ok(())
}

fn reserved_dedupe_session_suffix(dedupe_key: &str) -> Option<(&'static str, &str)> {
    for prefix in [ATTENTION_DEDUPE_KEY_PREFIX, TURN_DEDUPE_KEY_PREFIX] {
        if dedupe_key == prefix {
            return Some((prefix, ""));
        }
        if let Some(suffix) = dedupe_key
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            return Some((prefix, suffix));
        }
    }
    None
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
        // A record never takes `Unknown` as a status.
        NotificationStatus::Unread | NotificationStatus::Unknown => {}
        NotificationStatus::Read => record.read_at = Some(now.to_owned()),
        NotificationStatus::Acknowledged => record.acked_at = Some(now.to_owned()),
        NotificationStatus::Archived => record.archived_at = Some(now.to_owned()),
        NotificationStatus::Deleted => record.deleted_at = Some(now.to_owned()),
    }
}

fn same_source_namespace(left: &NotificationSource, right: &NotificationSource) -> bool {
    left.provider == right.provider && left.provider_event == right.provider_event
}

/// Whether `kind` is an attention notification that self-resolves on resume.
///
/// Only `agent_blocked` and `approval_required` represent a transient
/// waiting-for-owner condition, and only they are stored.
pub(crate) fn is_attention_kind(kind: NotificationKind) -> bool {
    matches!(
        kind,
        NotificationKind::AgentBlocked | NotificationKind::ApprovalRequired
    )
}

/// Whether `kind` is a turn report (`turn_completed`, `error`): a signal for
/// `session.wait_turn` that is never stored.
pub(crate) fn is_turn_report_kind(kind: NotificationKind) -> bool {
    matches!(
        kind,
        NotificationKind::TurnCompleted | NotificationKind::Error
    )
}

/// Whether a create should be deferred through the notification debounce coordinator.
///
/// Session attention creates and turn signals are held briefly so owner
/// activity can consume them before they become visible.
pub(crate) fn is_debounced_create(kind: NotificationKind, dedupe_key: Option<&str>) -> bool {
    is_attention_kind(kind) && dedupe_key.is_some_and(is_attention_dedupe_key)
        || kind == NotificationKind::TurnCompleted && dedupe_key.is_some_and(is_turn_dedupe_key)
}

/// Whether `dedupe_key` is a session attention dedupe key (`attention:<id>`).
pub(crate) fn is_attention_dedupe_key(dedupe_key: &str) -> bool {
    reserved_dedupe_session_suffix(dedupe_key)
        .is_some_and(|(prefix, suffix)| prefix == ATTENTION_DEDUPE_KEY_PREFIX && !suffix.is_empty())
}

/// Whether `dedupe_key` is a session turn dedupe key (`turn:<id>`).
pub(crate) fn is_turn_dedupe_key(dedupe_key: &str) -> bool {
    reserved_dedupe_session_suffix(dedupe_key)
        .is_some_and(|(prefix, suffix)| prefix == TURN_DEDUPE_KEY_PREFIX && !suffix.is_empty())
}

/// The provider name a create stores and ranks for the `raw` name it reports.
///
/// Control characters are dropped, whitespace runs collapse, and the name is
/// bounded, like every other source field.
pub(crate) fn stored_provider(raw: &str) -> String {
    normalize_user_string(raw, MAX_SOURCE_FIELD_CHARS)
}

/// Whether `provider` names a runtime whose hooks report notifications.
///
/// A provider is a hook provider exactly when some registered hook schema
/// lists it as an identity provider. `notification.create` checks a session
/// report from such a provider against the session's hook schema, and the
/// source ranking places its records above user and projector records.
pub(crate) fn is_hook_provider(provider: &str) -> bool {
    pohunek_worker_protocol::hook_schemas()
        .iter()
        .any(|schema| schema.identity_providers.contains(&provider))
}

/// Rank of a notification source when reports of one session moment conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePriority {
    /// The daemon's own projector (`pohunek` or `daemon`).
    Projector,
    /// Any other producer: a user, script, or external tool.
    User,
    /// A hook provider (see [`is_hook_provider`]) or a report the daemon
    /// admitted as a hook claim of its session's runtime.
    Provider,
}

fn source_priority(record: &NotificationRecord) -> SourcePriority {
    let source = &record.source;
    if record.hook_admitted || is_hook_provider(&source.provider) {
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
