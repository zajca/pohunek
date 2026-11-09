//! Append-only notification store.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use protocol::{
    NotificationKind, NotificationListParams, NotificationListResult, NotificationRecord,
    NotificationStatus,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{
    apply_status, inside_attention_window, is_attention_dedupe_key, is_attention_kind,
    parse_timestamp, same_source_namespace, source_priority, NotificationError, ResolveAsOf,
    SourcePriority,
};

/// Directory under the daemon data dir holding notification state.
pub const NOTIFICATIONS_SUBDIR: &str = "notifications";

/// File name of the append-only notification action log.
const NOTIFICATIONS_LOG_NAME: &str = "notifications.jsonl";

/// Temporary log name used for the atomic rewrite of the notification store.
const NOTIFICATIONS_REWRITE_NAME: &str = "notifications.jsonl.tmp";

/// Copy of the log kept before the first rewrite by a format-3 build.
pub(crate) const PRE_FORMAT_3_COPY_NAME: &str = "notifications.jsonl.pre-format-3";

/// Temporary name of the pre-format-3 copy while it is written.
const PRE_FORMAT_3_COPY_TEMP_NAME: &str = "notifications.jsonl.pre-format-3.tmp";

/// File name of the notification format marker.
///
/// A sidecar rather than a field of the action log: builds that predate it
/// ignore the file, and a build that meets a newer value stops using the store.
const FORMAT_FILE_NAME: &str = "format.json";

/// Format of the notification action log this build writes; it reads this
/// format and every earlier one. Format 3 has no policy document.
///
/// Raise it, together with a snapshot in `format_guard`, when a key or enum
/// value of `StoreAction` or `NotificationRecord` changes.
pub(crate) const NOTIFICATIONS_FORMAT_VERSION: u32 = 3;

#[cfg(unix)]
const OWNER_PRIVATE_DIR_MODE: u32 = 0o700;

#[cfg(unix)]
const OWNER_PRIVATE_FILE_MODE: u32 = 0o600;

/// Separator used by stable notification list cursors.
///
/// RFC3339 timestamps and daemon notification ids do not contain this byte, so
/// the cursor can stay compact without JSON/base64 encoding.
const CURSOR_SEPARATOR: char = '|';

/// Whether replay may ignore one unterminated EOF parse error at the log tail.
///
/// A daemon crash can interrupt an append after bytes are written but before the
/// newline-delimited JSON action is complete. Earlier parse failures or
/// newline-terminated failures may hide durable lifecycle actions and must stop
/// replay instead of reconstructing false state.
const TOLERATE_TRAILING_EOF_ACTION: bool = true;

/// Append-only notification store.
#[derive(Debug)]
pub(crate) struct NotificationStore {
    dir: PathBuf,
    path: PathBuf,
    state: Mutex<StoreState>,
    /// Why this build does not use the store. The files stay as they are, no
    /// append handle is open and every operation that needs the store fails.
    unavailable: Option<String>,
}

#[derive(Debug)]
struct StoreState {
    /// The append handle; `None` while the store is unavailable.
    file: Option<File>,
    records: BTreeMap<String, NotificationRecord>,
    action_count: usize,
    /// When another producer than the projector last contributed to a record:
    /// created it, or had a report merge into it. In memory only; a record
    /// without an entry counts as contributed to before any moment.
    contributions: HashMap<String, Instant>,
}

#[derive(Debug)]
struct ReplayState {
    records: BTreeMap<String, NotificationRecord>,
    action_count: usize,
    /// Length of the log up to its last complete action, set when an
    /// unterminated partial last line was ignored.
    valid_len: Option<u64>,
}

#[cfg(test)]
mod format_guard;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum StoreAction {
    Created {
        record: NotificationRecord,
    },
    CreatedWithUpdates {
        record: NotificationRecord,
        updated: Vec<NotificationRecord>,
    },
    Updated {
        record: NotificationRecord,
    },
    Deleted {
        record: NotificationRecord,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateOutcome {
    Created(NotificationRecord),
    Existing(NotificationRecord),
    Updated(NotificationRecord),
}

impl NotificationStore {
    /// Open the notification store under `data_dir`.
    ///
    /// Creates `<data_dir>/notifications` owner-private and opens
    /// `notifications.jsonl` owner-private in append mode.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when directory, file, or replay I/O fails.
    pub(crate) fn open(data_dir: &Path) -> Result<Self, NotificationError> {
        let dir = data_dir.join(NOTIFICATIONS_SUBDIR);
        create_private_dir(&dir)?;
        let path = dir.join(NOTIFICATIONS_LOG_NAME);
        if let Some(reason) = unsupported_format(&dir)? {
            return Ok(Self::unavailable(dir, path, reason));
        }
        let file = owner_private_append_options()
            .open(&path)
            .map_err(|source| NotificationError::io(&path, source))?;
        set_owner_private_file_permissions(&path)?;
        let replayed = match replay(&path) {
            Ok(replayed) => replayed,
            Err(
                error @ (NotificationError::StoreParse { .. }
                | NotificationError::Unavailable { .. }),
            ) => {
                drop(file);
                return Ok(Self::unavailable(dir, path, error.to_string()));
            }
            Err(error) => return Err(error),
        };
        if let Some(valid_len) = replayed.valid_len {
            // An append after the partial last line would glue onto it.
            file.set_len(valid_len)
                .map_err(|source| NotificationError::io(&path, source))?;
            file.sync_all()
                .map_err(|source| NotificationError::io(&path, source))?;
        }
        write_format_marker(&dir)?;
        Ok(Self {
            dir,
            path,
            state: Mutex::new(StoreState {
                file: Some(file),
                records: replayed.records,
                action_count: replayed.action_count,
                contributions: HashMap::new(),
            }),
            unavailable: None,
        })
    }

    fn unavailable(dir: PathBuf, path: PathBuf, reason: String) -> Self {
        warn!(
            file.path = %path.display(),
            reason = %reason,
            "the notification store is not used: notifications are unavailable and the \
             store files are left untouched; upgrade pohunek to a build that reads this \
             format, or move the notification directory aside to start a new store"
        );
        Self {
            dir,
            path,
            state: Mutex::new(StoreState {
                file: None,
                records: BTreeMap::new(),
                action_count: 0,
                contributions: HashMap::new(),
            }),
            unavailable: Some(reason),
        }
    }

    /// Fails with [`NotificationError::Unavailable`] while this build does not
    /// use the store.
    pub(crate) fn ensure_available(&self) -> Result<(), NotificationError> {
        match &self.unavailable {
            Some(reason) => Err(NotificationError::unavailable(reason.clone())),
            None => Ok(()),
        }
    }

    /// The backing JSONL file path.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The source ids of `provider`'s records that start with `prefix`, with
    /// the prefix removed, each with its record's creation timestamp.
    pub(crate) fn source_id_suffixes(&self, provider: &str, prefix: &str) -> Vec<(String, String)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .records
            .values()
            .filter(|record| record.source.provider == provider)
            .filter_map(|record| {
                let suffix = record.source_id.as_deref()?.strip_prefix(prefix)?;
                Some((suffix.to_owned(), record.created_at.clone()))
            })
            .collect()
    }

    /// Whether an unread attention record (`agent_blocked` or
    /// `approval_required`) carries `dedupe_key`.
    pub(crate) fn has_unread_attention(&self, dedupe_key: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.records.values().any(|record| {
            record.status == NotificationStatus::Unread
                && is_attention_kind(record.kind)
                && record.dedupe_key.as_deref() == Some(dedupe_key)
        })
    }

    /// Return all reconstructed notification records.
    pub(crate) fn all(&self) -> Vec<NotificationRecord> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.records.values().cloned().collect()
    }

    /// Whether `session_id` has an `agent_blocked` or `approval_required`
    /// record the owner has not acknowledged, archived or deleted.
    pub(crate) fn has_active_attention(&self, session_id: &protocol::SessionId) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.records.values().any(|record| {
            record.session_id.as_ref() == Some(session_id)
                && matches!(
                    record.kind,
                    NotificationKind::AgentBlocked | NotificationKind::ApprovalRequired
                )
                && matches!(
                    record.status,
                    NotificationStatus::Unread | NotificationStatus::Read
                )
        })
    }

    /// Removes the records that do not belong in the store and rewrites the
    /// log atomically when there are any.
    ///
    /// Removed: every record of a kind the inbox does not hold, every deleted
    /// record, and every resolved attention record whose resolution is at least
    /// `ttl_secs` old at `now`. Open attention records are never removed. The
    /// replacement file is fully flushed and opened for future appends before
    /// its atomic rename, so a crash leaves either the old replayable log or the
    /// complete new one. Before the first rewrite the old log is copied to
    /// [`PRE_FORMAT_3_COPY_NAME`].
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when a timestamp cannot be read, or when
    /// serialization, file I/O, atomic replacement or parent-directory syncing
    /// fails. The old log is intact in every error case.
    pub(crate) fn sweep(
        &self,
        now: time::OffsetDateTime,
        ttl_secs: u64,
    ) -> Result<Vec<NotificationRecord>, NotificationError> {
        self.ensure_available()?;
        let ttl = time::Duration::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = Vec::new();
        let mut retained = Vec::new();
        for record in state.records.values() {
            if record_expired(record, now, ttl)? {
                removed.push(record.clone());
            } else {
                retained.push(record.clone());
            }
        }
        if removed.is_empty() && state.action_count <= retained.len() {
            return Ok(removed);
        }

        let backup_path = self.dir.join(PRE_FORMAT_3_COPY_NAME);
        if !backup_path.exists() {
            // Copied through a synced temporary file, so a crash never leaves a
            // truncated copy that the existence check would keep.
            let backup_temp = self.dir.join(PRE_FORMAT_3_COPY_TEMP_NAME);
            fs::copy(&self.path, &backup_temp)
                .map_err(|source| NotificationError::io(&backup_temp, source))?;
            set_owner_private_file_permissions(&backup_temp)?;
            File::open(&backup_temp)
                .and_then(|copy| copy.sync_all())
                .map_err(|source| NotificationError::io(&backup_temp, source))?;
            fs::rename(&backup_temp, &backup_path)
                .map_err(|source| NotificationError::io(&backup_path, source))?;
            File::open(&self.dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| NotificationError::io(&self.dir, source))?;
        }
        let temp_path = self.dir.join(NOTIFICATIONS_REWRITE_NAME);
        let mut replacement = owner_private_write_options()
            .open(&temp_path)
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        for record in &retained {
            let mut line = serde_json::to_string(&StoreAction::Created {
                record: record.clone(),
            })
            .map_err(NotificationError::serialize)?;
            line.push('\n');
            replacement
                .write_all(line.as_bytes())
                .map_err(|source| NotificationError::io(&temp_path, source))?;
        }
        replacement
            .flush()
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        replacement
            .sync_all()
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        set_owner_private_file_permissions(&temp_path)?;
        drop(replacement);
        let replacement = owner_private_append_options()
            .open(&temp_path)
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        fs::rename(&temp_path, &self.path)
            .map_err(|source| NotificationError::io(&self.path, source))?;

        state.file = Some(replacement);
        state.action_count = retained.len();
        state.records = retained
            .into_iter()
            .map(|record| (record.id.0.clone(), record))
            .collect();
        File::open(&self.dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| NotificationError::io(&self.dir, source))?;
        Ok(removed)
    }

    /// Create a record or return/update a deduped visible record atomically.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when timestamp parsing, serialization, or
    /// writing fails.
    pub(crate) fn create_or_dedupe(
        &self,
        candidate: NotificationRecord,
        attention_window_secs: u64,
        minted: Instant,
    ) -> Result<CreateOutcome, NotificationError> {
        self.ensure_available()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(existing) = find_existing_source(&state.records, &candidate) {
            return Ok(CreateOutcome::Existing(existing));
        }

        if let Some(dedupe_key) = candidate.dedupe_key.as_deref() {
            let incoming_created_at = parse_timestamp(&candidate.created_at)?;
            let incoming_priority = source_priority(&candidate);
            for existing in state.records.values().cloned() {
                if !matches!(
                    existing.status,
                    NotificationStatus::Unread | NotificationStatus::Read
                ) {
                    continue;
                }
                if existing.dedupe_key.as_deref() != Some(dedupe_key) {
                    continue;
                }
                if !inside_attention_window(&existing, incoming_created_at, attention_window_secs)?
                {
                    continue;
                }
                let existing_priority = source_priority(&existing);
                match (incoming_priority, existing_priority) {
                    (SourcePriority::Provider, SourcePriority::Projector) => {
                        let updated = upgrade_projector(existing, &candidate);
                        append_action_locked(
                            &self.path,
                            &mut state,
                            StoreAction::Updated {
                                record: updated.clone(),
                            },
                        )?;
                        note_contribution(&mut state, &candidate, &updated.id.0, minted);
                        return Ok(CreateOutcome::Updated(updated));
                    }
                    (
                        SourcePriority::Projector | SourcePriority::Provider,
                        SourcePriority::Provider,
                    )
                    | (SourcePriority::Projector, SourcePriority::Projector) => {
                        note_contribution(&mut state, &candidate, &existing.id.0, minted);
                        return Ok(CreateOutcome::Existing(existing));
                    }
                    (SourcePriority::User, _)
                    | (
                        SourcePriority::Provider | SourcePriority::Projector,
                        SourcePriority::User,
                    ) => {}
                }
            }
        }

        append_action_locked(
            &self.path,
            &mut state,
            StoreAction::Created {
                record: candidate.clone(),
            },
        )?;
        note_contribution(&mut state, &candidate, &candidate.id.0, minted);
        Ok(CreateOutcome::Created(candidate))
    }

    /// Acknowledge active session notifications sharing `dedupe_key`.
    ///
    /// For `attention:<session_id>`, marks `unread` and `read`
    /// `agent_blocked`/`approval_required` records as `acknowledged`. Records
    /// already acknowledged, archived, deleted, or unrelated to the key prefix
    /// are left untouched, so repeated calls are
    /// idempotent. With `as_of`, records that other producers created, or merged
    /// a report into, after that moment are left untouched too.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when appending an update action fails.
    pub(crate) fn resolve_session_notifications(
        &self,
        dedupe_key: &str,
        now: &str,
        as_of: Option<&ResolveAsOf>,
    ) -> Result<Vec<NotificationRecord>, NotificationError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Collect matching ids first so the records map is not mutated while it
        // is being iterated below.
        let matching_ids: Vec<String> = state
            .records
            .values()
            .filter(|record| {
                record.dedupe_key.as_deref() == Some(dedupe_key)
                    && resolve_key_matches_kind(dedupe_key, record.kind)
                    && matches!(
                        record.status,
                        NotificationStatus::Unread | NotificationStatus::Read
                    )
                    && existed_at(&state, record, as_of)
            })
            .map(|record| record.id.0.clone())
            .collect();

        let mut resolved = Vec::with_capacity(matching_ids.len());
        for id in matching_ids {
            let Some(mut record) = state.records.get(&id).cloned() else {
                continue;
            };
            apply_status(&mut record, NotificationStatus::Acknowledged, now);
            append_action_locked(
                &self.path,
                &mut state,
                StoreAction::Updated {
                    record: record.clone(),
                },
            )?;
            resolved.push(record);
        }
        // A record that is no longer active needs no contribution time.
        let state = &mut *state;
        state.contributions.retain(|id, _| {
            state.records.get(id).is_some_and(|record| {
                matches!(
                    record.status,
                    NotificationStatus::Unread | NotificationStatus::Read
                )
            })
        });
        Ok(resolved)
    }

    /// List records using protocol filters and a stable cursor.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when timestamp filters or the cursor are
    /// invalid.
    pub(crate) fn list(
        &self,
        params: NotificationListParams,
    ) -> Result<NotificationListResult, NotificationError> {
        self.ensure_available()?;
        let NotificationListParams {
            status,
            kind,
            severity,
            provider,
            session_id,
            created_after,
            created_before,
            limit,
            cursor,
        } = params;
        let created_after = parse_optional_timestamp(created_after.as_deref())?;
        let created_before = parse_optional_timestamp(created_before.as_deref())?;
        let cursor = cursor.as_deref().map(parse_cursor).transpose()?;

        let mut records: Vec<NotificationRecord> = self
            .all()
            .into_iter()
            .filter(|record| is_attention_kind(record.kind))
            .filter(|record| {
                status.map_or(record.status != NotificationStatus::Deleted, |status| {
                    record.status == status
                }) && kind.is_none_or(|kind| record.kind == kind)
                    && severity.is_none_or(|severity| record.severity == severity)
                    && provider
                        .as_ref()
                        .is_none_or(|provider| record.source.provider == *provider)
                    && session_id
                        .as_ref()
                        .is_none_or(|session_id| record.session_id.as_ref() == Some(session_id))
            })
            .filter(|record| {
                record_in_time_range(record, created_after, created_before).unwrap_or(false)
            })
            .collect();

        sort_records_for_list(&mut records);
        if let Some(cursor) = cursor {
            records.retain(|record| record_after_cursor(record, &cursor));
        }

        let limit = limit.and_then(|value| usize::try_from(value).ok());
        let next_cursor = limit.and_then(|limit| {
            if records.len() > limit && limit > 0 {
                records.get(limit - 1).map(encode_cursor)
            } else {
                None
            }
        });
        if let Some(limit) = limit {
            records.truncate(limit);
        }

        Ok(NotificationListResult {
            notifications: records,
            next_cursor,
        })
    }
}

fn append_action_locked(
    path: &Path,
    state: &mut StoreState,
    action: StoreAction,
) -> Result<(), NotificationError> {
    let mut line = serde_json::to_string(&action).map_err(NotificationError::serialize)?;
    line.push('\n');
    let file = state
        .file
        .as_mut()
        .ok_or_else(|| NotificationError::unavailable("the notification store is not open"))?;
    file.write_all(line.as_bytes())
        .map_err(|source| NotificationError::io(path, source))?;
    file.flush()
        .map_err(|source| NotificationError::io(path, source))?;
    apply_action(&mut state.records, action);
    state.action_count += 1;
    Ok(())
}

/// Records that `candidate`, when it is not the projector's, contributed to the
/// record `id` at `minted`, keeping the newest contribution.
fn note_contribution(
    state: &mut StoreState,
    candidate: &NotificationRecord,
    id: &str,
    minted: Instant,
) {
    if source_priority(candidate) != SourcePriority::Projector {
        let newest = state.contributions.entry(id.to_owned()).or_insert(minted);
        *newest = (*newest).max(minted);
    }
}

/// Whether `record` existed, with every contribution it holds, when a resolve
/// `as_of` an earlier moment is applied.
///
/// The projector derives its records in the order its inputs arrive, so one
/// that is visible to a resolve always precedes that resolve's input; it holds
/// no contribution. Other producers are ordered by the instant they minted the
/// record's newest contribution.
fn existed_at(
    state: &StoreState,
    record: &NotificationRecord,
    as_of: Option<&ResolveAsOf>,
) -> bool {
    as_of.is_none_or(|as_of| {
        state
            .contributions
            .get(&record.id.0)
            .is_none_or(|contributed| *contributed <= as_of.minted_before)
    })
}

fn find_existing_source(
    records: &BTreeMap<String, NotificationRecord>,
    candidate: &NotificationRecord,
) -> Option<NotificationRecord> {
    let source_id = candidate.source_id.as_deref()?;
    records.values().find_map(|record| {
        (record.status != NotificationStatus::Deleted
            && record.source_id.as_deref() == Some(source_id)
            && same_source_namespace(&record.source, &candidate.source))
        .then(|| record.clone())
    })
}

fn resolve_key_matches_kind(dedupe_key: &str, kind: NotificationKind) -> bool {
    is_attention_dedupe_key(dedupe_key) && is_attention_kind(kind)
}

/// Whether the sweep removes `record` at `now`.
fn record_expired(
    record: &NotificationRecord,
    now: time::OffsetDateTime,
    ttl: time::Duration,
) -> Result<bool, NotificationError> {
    if !is_attention_kind(record.kind) || record.status == NotificationStatus::Deleted {
        return Ok(true);
    }
    let resolved_at = match record.status {
        NotificationStatus::Acknowledged => record.acked_at.as_deref(),
        NotificationStatus::Archived => record.archived_at.as_deref(),
        _ => return Ok(false),
    };
    let resolved_at = parse_timestamp(resolved_at.unwrap_or(&record.created_at))?;
    Ok(resolved_at + ttl <= now)
}

fn upgrade_projector(
    mut existing: NotificationRecord,
    replacement: &NotificationRecord,
) -> NotificationRecord {
    existing.source.clone_from(&replacement.source);
    existing.kind = replacement.kind;
    existing.severity = replacement.severity;
    existing.title.clone_from(&replacement.title);
    existing.body.clone_from(&replacement.body);
    existing.metadata.clone_from(&replacement.metadata);
    existing.session_id.clone_from(&replacement.session_id);
    existing.agent_kind.clone_from(&replacement.agent_kind);
    existing.source_id.clone_from(&replacement.source_id);
    existing.dedupe_key.clone_from(&replacement.dedupe_key);
    existing.project_id.clone_from(&replacement.project_id);
    existing.hook_admitted = replacement.hook_admitted;
    existing.superseded_by = None;
    existing
}

/// Why the marker in `dir` names a format this build does not read, if it does.
///
/// A missing marker is format 1.
fn unsupported_format(dir: &Path) -> Result<Option<String>, NotificationError> {
    let path = dir.join(FORMAT_FILE_NAME);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) if source.kind() == io::ErrorKind::InvalidData => {
            return Ok(Some(format!(
                "the notification format marker {} is not text",
                path.display()
            )))
        }
        Err(source) => return Err(NotificationError::io(&path, source)),
    };
    let marker = match serde_json::from_str::<FormatMarker>(&content) {
        Ok(marker) => marker,
        Err(error) => {
            return Ok(Some(format!(
                "the notification format marker {} cannot be read: {error}",
                path.display()
            )))
        }
    };
    if marker.format_version > NOTIFICATIONS_FORMAT_VERSION {
        return Ok(Some(format!(
            "the notification store uses format {}, newer than the format {} this build reads",
            marker.format_version, NOTIFICATIONS_FORMAT_VERSION
        )));
    }
    Ok(None)
}

/// Writes the format marker atomically when it is missing or names an older
/// format than this build writes.
///
/// A marker that is present was checked by [`unsupported_format`] to be
/// readable and not newer than this build.
fn write_format_marker(dir: &Path) -> Result<(), NotificationError> {
    let path = dir.join(FORMAT_FILE_NAME);
    if path.exists() {
        let content =
            fs::read_to_string(&path).map_err(|source| NotificationError::io(&path, source))?;
        let current = serde_json::from_str::<FormatMarker>(&content)
            .is_ok_and(|marker| marker.format_version >= NOTIFICATIONS_FORMAT_VERSION);
        if current {
            return Ok(());
        }
    }
    let temp_path = dir.join("format.json.tmp");
    let mut file = owner_private_write_options()
        .open(&temp_path)
        .map_err(|source| NotificationError::io(&temp_path, source))?;
    serde_json::to_writer(
        &mut file,
        &FormatMarker {
            format_version: NOTIFICATIONS_FORMAT_VERSION,
        },
    )
    .map_err(NotificationError::serialize)?;
    file.write_all(b"\n")
        .map_err(|source| NotificationError::io(&temp_path, source))?;
    file.sync_all()
        .map_err(|source| NotificationError::io(&temp_path, source))?;
    set_owner_private_file_permissions(&temp_path)?;
    fs::rename(&temp_path, &path).map_err(|source| NotificationError::io(&path, source))?;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| NotificationError::io(dir, source))
}

/// The `format.json` document.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatMarker {
    format_version: u32,
}

fn replay(path: &Path) -> Result<ReplayState, NotificationError> {
    let content = fs::read_to_string(path).map_err(|source| {
        if source.kind() == io::ErrorKind::InvalidData {
            NotificationError::unavailable(format!(
                "the notification log {} is not text",
                path.display()
            ))
        } else {
            NotificationError::io(path, source)
        }
    })?;
    let mut records = BTreeMap::new();
    let mut action_count = 0;
    let mut valid_len = None;
    let mut offset: u64 = 0;
    for (index, raw_line) in content.split_inclusive('\n').enumerate() {
        let line_start = offset;
        offset += raw_line.len() as u64;
        let line_number = index + 1;
        let terminated = raw_line.ends_with('\n');
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StoreAction>(line) {
            Ok(action) => {
                apply_action(&mut records, action);
                action_count += 1;
            }
            Err(err) if is_tolerated_trailing_partial(terminated, &err) => {
                warn!(
                    error = %err,
                    file.path = %path.display(),
                    line.number = line_number,
                    "ignoring truncated trailing notification-store action"
                );
                valid_len = Some(line_start);
            }
            Err(err) => return Err(NotificationError::store_parse(path, line_number, err)),
        }
    }
    Ok(ReplayState {
        records,
        action_count,
        valid_len,
    })
}

fn is_tolerated_trailing_partial(terminated: bool, err: &serde_json::Error) -> bool {
    TOLERATE_TRAILING_EOF_ACTION
        && !terminated
        && err.classify() == serde_json::error::Category::Eof
}

fn apply_action(records: &mut BTreeMap<String, NotificationRecord>, action: StoreAction) {
    match action {
        StoreAction::Created { record }
        | StoreAction::Updated { record }
        | StoreAction::Deleted { record } => {
            records.insert(record.id.0.clone(), record);
        }
        StoreAction::CreatedWithUpdates { record, updated } => {
            records.insert(record.id.0.clone(), record);
            for record in updated {
                records.insert(record.id.0.clone(), record);
            }
        }
    }
}

fn sort_records_for_list(records: &mut [NotificationRecord]) {
    records.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
}

fn parse_optional_timestamp(
    value: Option<&str>,
) -> Result<Option<time::OffsetDateTime>, NotificationError> {
    value.map(parse_timestamp).transpose()
}

fn record_in_time_range(
    record: &NotificationRecord,
    created_after: Option<time::OffsetDateTime>,
    created_before: Option<time::OffsetDateTime>,
) -> Result<bool, NotificationError> {
    let created_at = parse_timestamp(&record.created_at)?;
    Ok(created_after.is_none_or(|after| created_at >= after)
        && created_before.is_none_or(|before| created_at < before))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    created_at: String,
    id: String,
}

fn encode_cursor(record: &NotificationRecord) -> String {
    format!("{}{CURSOR_SEPARATOR}{}", record.created_at, record.id.0)
}

fn parse_cursor(cursor: &str) -> Result<Cursor, NotificationError> {
    let Some((created_at, id)) = cursor.split_once(CURSOR_SEPARATOR) else {
        return Err(NotificationError::InvalidCursor {
            cursor: cursor.to_owned(),
        });
    };
    parse_timestamp(created_at)?;
    if id.is_empty() {
        return Err(NotificationError::InvalidCursor {
            cursor: cursor.to_owned(),
        });
    }
    Ok(Cursor {
        created_at: created_at.to_owned(),
        id: id.to_owned(),
    })
}

fn record_after_cursor(record: &NotificationRecord, cursor: &Cursor) -> bool {
    record.created_at < cursor.created_at
        || (record.created_at == cursor.created_at && record.id.0 > cursor.id)
}

fn owner_private_append_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OWNER_PRIVATE_FILE_MODE)
    };
    options
}

fn owner_private_write_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OWNER_PRIVATE_FILE_MODE)
    };
    options
}

fn create_private_dir(dir: &Path) -> Result<(), NotificationError> {
    fs::create_dir_all(dir).map_err(|source| NotificationError::io(dir, source))?;
    set_owner_private_dir_permissions(dir)
}

#[cfg(unix)]
fn set_owner_private_dir_permissions(dir: &Path) -> Result<(), NotificationError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(OWNER_PRIVATE_DIR_MODE))
        .map_err(|source| NotificationError::io(dir, source))
}

#[cfg(not(unix))]
fn set_owner_private_dir_permissions(_dir: &Path) -> Result<(), NotificationError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_private_file_permissions(path: &Path) -> Result<(), NotificationError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_PRIVATE_FILE_MODE))
        .map_err(|source| NotificationError::io(path, source))
}

#[cfg(not(unix))]
fn set_owner_private_file_permissions(_path: &Path) -> Result<(), NotificationError> {
    Ok(())
}
