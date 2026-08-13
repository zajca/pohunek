//! Append-only notification store.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::{
    NotificationId, NotificationKind, NotificationListParams, NotificationListResult,
    NotificationPolicy, NotificationRecord, NotificationStatus,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{
    apply_status, inside_attention_window, is_attention_dedupe_key, is_attention_kind,
    is_turn_dedupe_key, is_valid_status_transition, parse_timestamp, same_source_namespace,
    source_priority, NotificationError, SourcePriority, TURN_DEDUPE_KEY_PREFIX,
};

/// Directory under the daemon data dir holding notification state.
pub const NOTIFICATIONS_SUBDIR: &str = "notifications";

/// File name of the append-only notification action log.
const NOTIFICATIONS_LOG_NAME: &str = "notifications.jsonl";

/// Temporary log name used for atomic notification-store compaction.
const NOTIFICATIONS_COMPACTION_NAME: &str = "notifications.jsonl.tmp";

/// File name of the durable notification policy.
///
/// Policy is singleton configuration state, not notification audit history, so
/// it is atomically replaced beside the append-only action log instead of being
/// replayed from notification actions.
const POLICY_FILE_NAME: &str = "policy.json";

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
}

#[derive(Debug)]
struct StoreState {
    file: File,
    records: BTreeMap<String, NotificationRecord>,
    action_count: usize,
}

#[derive(Debug)]
struct ReplayState {
    records: BTreeMap<String, NotificationRecord>,
    action_count: usize,
}

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
    CreatedWithUpdates {
        record: NotificationRecord,
        updated: Vec<NotificationRecord>,
    },
    Existing(NotificationRecord),
    Updated(NotificationRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateOutcome {
    Updated(NotificationRecord),
    Deleted(NotificationRecord),
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
        let file = owner_private_append_options()
            .open(&path)
            .map_err(|source| NotificationError::io(&path, source))?;
        set_owner_private_file_permissions(&path)?;
        let replayed = replay(&path)?;
        Ok(Self {
            dir,
            path,
            state: Mutex::new(StoreState {
                file,
                records: replayed.records,
                action_count: replayed.action_count,
            }),
        })
    }

    /// The backing JSONL file path.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Return all reconstructed notification records.
    pub(crate) fn all(&self) -> Vec<NotificationRecord> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.records.values().cloned().collect()
    }

    /// Physically rewrite the action log to its current visible records.
    ///
    /// Compaction is skipped below `minimum_actions` and when it would not
    /// remove an update or tombstone. The replacement file is fully flushed and
    /// opened for future appends before its atomic rename, so a crash leaves
    /// either the old replayable log or the complete compacted log.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when serialization, file I/O, atomic
    /// replacement, or parent-directory syncing fails.
    pub(crate) fn compact_if_needed(
        &self,
        minimum_actions: u32,
    ) -> Result<bool, NotificationError> {
        let minimum_actions = usize::try_from(minimum_actions).unwrap_or(usize::MAX);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visible_records: Vec<NotificationRecord> = state
            .records
            .values()
            .filter(|record| record.status != NotificationStatus::Deleted)
            .cloned()
            .collect();
        if state.action_count < minimum_actions || state.action_count <= visible_records.len() {
            return Ok(false);
        }

        let temp_path = self.dir.join(NOTIFICATIONS_COMPACTION_NAME);
        let mut replacement = owner_private_write_options()
            .open(&temp_path)
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        for record in &visible_records {
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

        state.file = replacement;
        state
            .records
            .retain(|_, record| record.status != NotificationStatus::Deleted);
        state.action_count = visible_records.len();
        File::open(&self.dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| NotificationError::io(&self.dir, source))?;
        Ok(true)
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
    ) -> Result<CreateOutcome, NotificationError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(existing) = find_existing_source(&state.records, &candidate) {
            return Ok(CreateOutcome::Existing(existing));
        }

        if let Some(dedupe_key) = candidate
            .dedupe_key
            .as_deref()
            .filter(|dedupe_key| is_turn_dedupe_key(dedupe_key))
            .filter(|_| candidate.kind == NotificationKind::TurnCompleted)
        {
            let updated = supersede_unread_records(
                &state.records,
                dedupe_key,
                candidate.kind,
                &candidate.id,
                &candidate.created_at,
            );
            if updated.is_empty() {
                append_action_locked(
                    &self.path,
                    &mut state,
                    StoreAction::Created {
                        record: candidate.clone(),
                    },
                )?;
                return Ok(CreateOutcome::Created(candidate));
            }
            append_action_locked(
                &self.path,
                &mut state,
                StoreAction::CreatedWithUpdates {
                    record: candidate.clone(),
                    updated: updated.clone(),
                },
            )?;
            return Ok(CreateOutcome::CreatedWithUpdates {
                record: candidate,
                updated,
            });
        }

        if let Some(dedupe_key) = candidate.dedupe_key.as_deref() {
            let incoming_created_at = parse_timestamp(&candidate.created_at)?;
            let incoming_priority = source_priority(&candidate.source);
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
                let existing_priority = source_priority(&existing.source);
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
                        return Ok(CreateOutcome::Updated(updated));
                    }
                    (
                        SourcePriority::Projector | SourcePriority::Provider,
                        SourcePriority::Provider,
                    )
                    | (SourcePriority::Projector, SourcePriority::Projector) => {
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
        Ok(CreateOutcome::Created(candidate))
    }

    /// Supersede unread `turn_completed` records consumed by visible attention.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when appending an update action fails.
    pub(crate) fn supersede_turns_for_attention(
        &self,
        attention: &NotificationRecord,
        now: &str,
    ) -> Result<Vec<NotificationRecord>, NotificationError> {
        if !is_attention_kind(attention.kind) {
            return Ok(Vec::new());
        }
        if !matches!(
            attention.status,
            NotificationStatus::Unread | NotificationStatus::Read
        ) {
            return Ok(Vec::new());
        }
        let Some(session_id) = attention.session_id.as_ref() else {
            return Ok(Vec::new());
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let turn_key = format!("{TURN_DEDUPE_KEY_PREFIX}:{}", session_id.0);
        supersede_unread_records_locked(
            &self.path,
            &mut state,
            &turn_key,
            NotificationKind::TurnCompleted,
            &attention.id,
            now,
        )
    }

    /// Load the persisted notification policy or return `fallback`.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when the policy file cannot be read or
    /// parsed.
    pub(crate) fn load_policy(
        &self,
        fallback: NotificationPolicy,
    ) -> Result<NotificationPolicy, NotificationError> {
        let path = self.policy_path();
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|source| NotificationError::policy_parse(path, source)),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(fallback),
            Err(source) => Err(NotificationError::io(path, source)),
        }
    }

    /// Atomically persist the notification policy.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when serialization or writing fails.
    pub(crate) fn write_policy(
        &self,
        policy: &NotificationPolicy,
    ) -> Result<(), NotificationError> {
        let path = self.policy_path();
        let temp_path = self.dir.join("policy.json.tmp");
        let mut file = owner_private_write_options()
            .open(&temp_path)
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        serde_json::to_writer_pretty(&mut file, policy).map_err(NotificationError::serialize)?;
        file.write_all(b"\n")
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        file.flush()
            .map_err(|source| NotificationError::io(&temp_path, source))?;
        set_owner_private_file_permissions(&temp_path)?;
        fs::rename(&temp_path, &path).map_err(|source| NotificationError::io(&path, source))?;
        set_owner_private_file_permissions(&path)?;
        Ok(())
    }

    /// Append a created action.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when serialization or writing fails.
    #[cfg(test)]
    pub(crate) fn append_created(
        &self,
        record: NotificationRecord,
    ) -> Result<(), NotificationError> {
        self.append_action(StoreAction::Created { record })
    }

    /// Append an updated action.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when serialization or writing fails.
    #[cfg(test)]
    pub(crate) fn append_updated(
        &self,
        record: NotificationRecord,
    ) -> Result<(), NotificationError> {
        self.append_action(StoreAction::Updated { record })
    }

    /// Append a deleted action.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when serialization or writing fails.
    #[cfg(test)]
    pub(crate) fn append_deleted(
        &self,
        record: NotificationRecord,
    ) -> Result<(), NotificationError> {
        self.append_action(StoreAction::Deleted { record })
    }

    /// Atomically validate, apply, and append an update transition.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when the id is unknown, the transition is
    /// invalid, or the append fails.
    pub(crate) fn update_transition(
        &self,
        id: NotificationId,
        status: NotificationStatus,
        now: &str,
    ) -> Result<UpdateOutcome, NotificationError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut record) = state.records.get(&id.0).cloned() else {
            return Err(NotificationError::NotFound { id });
        };
        if !is_valid_status_transition(record.status, status) {
            return Err(NotificationError::InvalidTransition {
                id,
                from: record.status,
                to: status,
            });
        }

        apply_status(&mut record, status, now);
        if record.status == NotificationStatus::Deleted {
            append_action_locked(
                &self.path,
                &mut state,
                StoreAction::Deleted {
                    record: record.clone(),
                },
            )?;
            Ok(UpdateOutcome::Deleted(record))
        } else {
            append_action_locked(
                &self.path,
                &mut state,
                StoreAction::Updated {
                    record: record.clone(),
                },
            )?;
            Ok(UpdateOutcome::Updated(record))
        }
    }

    /// Atomically apply and append a delete transition.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when the append fails.
    pub(crate) fn delete_transition(
        &self,
        id: &NotificationId,
        now: &str,
    ) -> Result<Option<NotificationRecord>, NotificationError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut record) = state.records.get(&id.0).cloned() else {
            return Ok(None);
        };
        if record.status == NotificationStatus::Deleted {
            return Ok(None);
        }

        apply_status(&mut record, NotificationStatus::Deleted, now);
        append_action_locked(
            &self.path,
            &mut state,
            StoreAction::Deleted {
                record: record.clone(),
            },
        )?;
        Ok(Some(record))
    }

    /// Acknowledge active session notifications sharing `dedupe_key`.
    ///
    /// For `attention:<session_id>`, marks `unread` and `read`
    /// `agent_blocked`/`approval_required` records as `acknowledged`. For
    /// `turn:<session_id>`, marks `unread` and `read` `turn_completed` records
    /// as `acknowledged`. Records already acknowledged, archived, deleted, or
    /// unrelated to the key prefix are left untouched, so repeated calls are
    /// idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`NotificationError`] when appending an update action fails.
    pub(crate) fn resolve_session_notifications(
        &self,
        dedupe_key: &str,
        now: &str,
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

    #[cfg(test)]
    fn append_action(&self, action: StoreAction) -> Result<(), NotificationError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        append_action_locked(&self.path, &mut state, action)
    }

    fn policy_path(&self) -> PathBuf {
        self.dir.join(POLICY_FILE_NAME)
    }
}

fn append_action_locked(
    path: &Path,
    state: &mut StoreState,
    action: StoreAction,
) -> Result<(), NotificationError> {
    let mut line = serde_json::to_string(&action).map_err(NotificationError::serialize)?;
    line.push('\n');
    state
        .file
        .write_all(line.as_bytes())
        .map_err(|source| NotificationError::io(path, source))?;
    state
        .file
        .flush()
        .map_err(|source| NotificationError::io(path, source))?;
    apply_action(&mut state.records, action);
    state.action_count += 1;
    Ok(())
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

fn supersede_unread_records_locked(
    path: &Path,
    state: &mut StoreState,
    dedupe_key: &str,
    kind: NotificationKind,
    superseded_by: &NotificationId,
    now: &str,
) -> Result<Vec<NotificationRecord>, NotificationError> {
    let updated = supersede_unread_records(&state.records, dedupe_key, kind, superseded_by, now);
    for record in &updated {
        append_action_locked(
            path,
            state,
            StoreAction::Updated {
                record: record.clone(),
            },
        )?;
    }
    Ok(updated)
}

fn supersede_unread_records(
    records: &BTreeMap<String, NotificationRecord>,
    dedupe_key: &str,
    kind: NotificationKind,
    superseded_by: &NotificationId,
    now: &str,
) -> Vec<NotificationRecord> {
    records
        .values()
        .filter(|record| {
            record.kind == kind
                && record.status == NotificationStatus::Unread
                && record.dedupe_key.as_deref() == Some(dedupe_key)
        })
        .cloned()
        .map(|mut record| {
            apply_status(&mut record, NotificationStatus::Acknowledged, now);
            record.superseded_by = Some(superseded_by.clone());
            record
        })
        .collect()
}

fn resolve_key_matches_kind(dedupe_key: &str, kind: NotificationKind) -> bool {
    if is_attention_dedupe_key(dedupe_key) {
        return is_attention_kind(kind);
    }
    is_turn_dedupe_key(dedupe_key) && kind == NotificationKind::TurnCompleted
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
    existing.superseded_by = None;
    existing
}

fn replay(path: &Path) -> Result<ReplayState, NotificationError> {
    let content = fs::read_to_string(path).map_err(|source| NotificationError::io(path, source))?;
    let mut records = BTreeMap::new();
    let mut action_count = 0;
    for (index, raw_line) in content.split_inclusive('\n').enumerate() {
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
            }
            Err(err) => return Err(NotificationError::store_parse(path, line_number, err)),
        }
    }
    Ok(ReplayState {
        records,
        action_count,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::{
        NotificationId, NotificationKind, NotificationSeverity, NotificationSource,
        NotificationStatus,
    };

    use super::NotificationStore;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_data_dir(tag: &str) -> std::path::PathBuf {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pohunek-notifications-store-{tag}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    fn record(id: &str, created_at: &str) -> protocol::NotificationRecord {
        protocol::NotificationRecord {
            id: NotificationId(id.to_owned()),
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: format!("source-{id}"),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::ActionRequired,
            status: NotificationStatus::Unread,
            title: format!("Title {id}"),
            body: format!("Body {id}"),
            metadata: BTreeMap::new(),
            created_at: created_at.to_owned(),
            session_id: None,
            agent_kind: None,
            source_id: Some(format!("source-id-{id}")),
            dedupe_key: None,
            project_id: None,
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        }
    }

    #[test]
    fn resolve_attention_acknowledges_matching_attention_records() {
        let data_dir = temp_data_dir("resolve-attention");
        let store = NotificationStore::open(&data_dir).expect("open store");

        let mut blocked = record("n-1", "2026-07-03T10:00:00Z");
        blocked.kind = NotificationKind::AgentBlocked;
        blocked.dedupe_key = Some("attention:s-1".to_owned());
        store.append_created(blocked).expect("append blocked");

        let mut approval = record("n-2", "2026-07-03T10:00:01Z");
        approval.kind = NotificationKind::ApprovalRequired;
        approval.dedupe_key = Some("attention:s-1".to_owned());
        store.append_created(approval).expect("append approval");

        // A different session must not be acknowledged.
        let mut other = record("n-3", "2026-07-03T10:00:02Z");
        other.kind = NotificationKind::AgentBlocked;
        other.dedupe_key = Some("attention:s-2".to_owned());
        store.append_created(other).expect("append other session");

        let resolved = store
            .resolve_session_notifications("attention:s-1", "2026-07-03T10:05:00Z")
            .expect("resolve attention");

        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|record| {
            record.status == NotificationStatus::Acknowledged
                && record.acked_at.as_deref() == Some("2026-07-03T10:05:00Z")
        }));

        let statuses: BTreeMap<String, NotificationStatus> = store
            .all()
            .into_iter()
            .map(|record| (record.id.0, record.status))
            .collect();
        assert_eq!(statuses["n-1"], NotificationStatus::Acknowledged);
        assert_eq!(statuses["n-2"], NotificationStatus::Acknowledged);
        assert_eq!(statuses["n-3"], NotificationStatus::Unread);
    }

    #[test]
    fn resolve_attention_skips_non_attention_kinds_and_is_idempotent() {
        let data_dir = temp_data_dir("resolve-attention-idempotent");
        let store = NotificationStore::open(&data_dir).expect("open store");

        let mut blocked = record("n-1", "2026-07-03T10:00:00Z");
        blocked.kind = NotificationKind::AgentBlocked;
        blocked.dedupe_key = Some("attention:s-1".to_owned());
        store.append_created(blocked).expect("append blocked");

        // An error notification shares the dedupe key but must not auto-resolve.
        let mut errored = record("n-2", "2026-07-03T10:00:01Z");
        errored.kind = NotificationKind::Error;
        errored.dedupe_key = Some("attention:s-1".to_owned());
        store.append_created(errored).expect("append error");

        let first = store
            .resolve_session_notifications("attention:s-1", "2026-07-03T10:05:00Z")
            .expect("first resolve");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id.0, "n-1");

        let second = store
            .resolve_session_notifications("attention:s-1", "2026-07-03T10:06:00Z")
            .expect("second resolve");
        assert!(second.is_empty());

        let statuses: BTreeMap<String, NotificationStatus> = store
            .all()
            .into_iter()
            .map(|record| (record.id.0, record.status))
            .collect();
        assert_eq!(statuses["n-1"], NotificationStatus::Acknowledged);
        assert_eq!(statuses["n-2"], NotificationStatus::Unread);
    }

    #[test]
    fn second_unread_turn_supersedes_previous_turn_without_time_window() {
        let data_dir = temp_data_dir("turn-supersede");
        let store = NotificationStore::open(&data_dir).expect("open store");

        let mut first = record("n-1", "2026-07-03T10:00:00Z");
        first.kind = NotificationKind::TurnCompleted;
        first.dedupe_key = Some("turn:s-1".to_owned());
        store
            .create_or_dedupe(first, 120)
            .expect("first turn create");

        let mut second = record("n-2", "2026-07-03T11:00:00Z");
        second.kind = NotificationKind::TurnCompleted;
        second.dedupe_key = Some("turn:s-1".to_owned());
        store
            .create_or_dedupe(second, 120)
            .expect("second turn create");

        let log = std::fs::read_to_string(store.path()).expect("read notification log");
        assert_eq!(
            log.lines().count(),
            2,
            "second turn supersede and replacement must be one durable action"
        );

        let reopened = NotificationStore::open(&data_dir).expect("reopen store");
        let records: BTreeMap<String, protocol::NotificationRecord> = store
            .all()
            .into_iter()
            .map(|record| (record.id.0.clone(), record))
            .collect();
        let replayed: BTreeMap<String, protocol::NotificationRecord> = reopened
            .all()
            .into_iter()
            .map(|record| (record.id.0.clone(), record))
            .collect();
        assert_eq!(records, replayed);
        assert_eq!(records["n-1"].status, NotificationStatus::Acknowledged);
        assert_eq!(
            records["n-1"].superseded_by,
            Some(NotificationId("n-2".to_owned()))
        );
        assert_eq!(records["n-2"].status, NotificationStatus::Unread);
        assert_eq!(records["n-2"].superseded_by, None);
    }

    #[test]
    fn resolve_session_notifications_acknowledges_turn_keyed_turns_only() {
        let data_dir = temp_data_dir("resolve-turn");
        let store = NotificationStore::open(&data_dir).expect("open store");

        let mut turn = record("n-1", "2026-07-03T10:00:00Z");
        turn.kind = NotificationKind::TurnCompleted;
        turn.dedupe_key = Some("turn:s-1".to_owned());
        store.append_created(turn).expect("append turn");

        let mut attention = record("n-2", "2026-07-03T10:00:01Z");
        attention.kind = NotificationKind::AgentBlocked;
        attention.dedupe_key = Some("turn:s-1".to_owned());
        store.append_created(attention).expect("append attention");

        let resolved = store
            .resolve_session_notifications("turn:s-1", "2026-07-03T10:05:00Z")
            .expect("resolve turn");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id.0, "n-1");
        assert_eq!(resolved[0].status, NotificationStatus::Acknowledged);

        let statuses: BTreeMap<String, NotificationStatus> = store
            .all()
            .into_iter()
            .map(|record| (record.id.0, record.status))
            .collect();
        assert_eq!(statuses["n-1"], NotificationStatus::Acknowledged);
        assert_eq!(statuses["n-2"], NotificationStatus::Unread);
    }

    #[test]
    fn open_creates_notifications_directory_and_file_owner_private() {
        let data_dir = temp_data_dir("permissions");
        let store = NotificationStore::open(&data_dir).expect("open store");

        assert!(data_dir.join("notifications").is_dir());
        assert!(store.path().is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir_mode = std::fs::metadata(data_dir.join("notifications"))
                .expect("dir metadata")
                .permissions()
                .mode()
                & 0o777;
            let file_mode = std::fs::metadata(store.path())
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn append_create_update_archive_delete_replays_after_reopen() {
        let data_dir = temp_data_dir("replay");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let mut first = record("n-1", "2026-07-03T10:00:00Z");
        store.append_created(first.clone()).expect("append created");

        first.status = NotificationStatus::Read;
        first.read_at = Some("2026-07-03T10:05:00Z".to_owned());
        store.append_updated(first.clone()).expect("append updated");

        first.status = NotificationStatus::Archived;
        first.archived_at = Some("2026-07-03T10:10:00Z".to_owned());
        store
            .append_updated(first.clone())
            .expect("append archived");

        first.status = NotificationStatus::Deleted;
        first.deleted_at = Some("2026-07-03T10:15:00Z".to_owned());
        store.append_deleted(first.clone()).expect("append deleted");

        let reopened = NotificationStore::open(&data_dir).expect("reopen store");
        let records = reopened.all();

        assert_eq!(records, vec![first]);
        let lines = std::fs::read_to_string(reopened.path())
            .expect("read log")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(lines, 4);
    }

    #[test]
    fn compaction_removes_history_and_tombstones_and_keeps_append_handle_live() {
        let data_dir = temp_data_dir("compaction");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let mut retained = record("n-1", "2026-07-03T10:00:00Z");
        store
            .append_created(retained.clone())
            .expect("append retained record");
        retained.status = NotificationStatus::Read;
        retained.read_at = Some("2026-07-03T10:01:00Z".to_owned());
        store
            .append_updated(retained.clone())
            .expect("append retained update");

        let mut deleted = record("n-2", "2026-07-03T10:02:00Z");
        store
            .append_created(deleted.clone())
            .expect("append deleted record");
        deleted.status = NotificationStatus::Deleted;
        deleted.deleted_at = Some("2026-07-03T10:03:00Z".to_owned());
        store
            .append_deleted(deleted)
            .expect("append delete tombstone");

        assert!(store.compact_if_needed(1).expect("compact store"));
        assert_eq!(
            std::fs::read_to_string(store.path())
                .expect("read compacted log")
                .lines()
                .count(),
            1
        );

        let appended = record("n-3", "2026-07-03T10:04:00Z");
        store
            .append_created(appended.clone())
            .expect("append after compaction");
        let reopened = NotificationStore::open(&data_dir).expect("reopen compacted store");

        assert_eq!(reopened.all(), vec![retained, appended]);
    }

    #[test]
    fn replay_tolerates_truncated_trailing_action_and_keeps_prior_records() {
        let data_dir = temp_data_dir("trailing-partial");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let first = record("n-1", "2026-07-03T10:00:00Z");
        store
            .append_created(first.clone())
            .expect("append created record");
        let path = store.path().to_path_buf();
        drop(store);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open log for trailing partial")
            .write_all(br#"{"action":"updated""#)
            .expect("write trailing partial");

        let reopened = NotificationStore::open(&data_dir).expect("reopen with trailing partial");

        assert_eq!(reopened.all(), vec![first]);
    }

    #[test]
    fn replay_rejects_corrupt_mid_file_action() {
        let data_dir = temp_data_dir("mid-file-corrupt");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let first = record("n-1", "2026-07-03T10:00:00Z");
        let second = record("n-2", "2026-07-03T10:05:00Z");
        store
            .append_created(first)
            .expect("append first created record");
        store
            .append_created(second)
            .expect("append second created record");
        let path = store.path().to_path_buf();
        drop(store);
        let content = std::fs::read_to_string(&path).expect("read original log");
        let mut lines = content.lines();
        let first_line = lines.next().expect("first log line");
        let second_line = lines.next().expect("second log line");
        std::fs::write(
            &path,
            format!("{first_line}\n{{not-json}}\n{second_line}\n"),
        )
        .expect("write corrupt mid-file log");

        let err = NotificationStore::open(&data_dir)
            .expect_err("mid-file corrupt action must fail store open");

        assert_eq!(err.to_protocol_error().code, "notification_store_error");
        match err {
            super::super::NotificationError::StoreParse { line, .. } => assert_eq!(line, 2),
            other => panic!("expected StoreParse error, got {other:?}"),
        }
    }

    #[test]
    fn replay_keeps_deleted_action_before_trailing_partial() {
        let data_dir = temp_data_dir("deleted-before-partial");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let mut deleted = record("n-1", "2026-07-03T10:00:00Z");
        store
            .append_created(deleted.clone())
            .expect("append created record");
        deleted.status = NotificationStatus::Deleted;
        deleted.deleted_at = Some("2026-07-03T10:05:00Z".to_owned());
        store
            .append_deleted(deleted.clone())
            .expect("append deleted record");
        let path = store.path().to_path_buf();
        drop(store);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open log for trailing partial")
            .write_all(br#"{"action":"updated""#)
            .expect("write trailing partial");

        let reopened = NotificationStore::open(&data_dir).expect("reopen with trailing partial");

        assert_eq!(reopened.all(), vec![deleted]);
    }

    #[test]
    fn list_filters_by_status_kind_severity_provider_session_and_time_range() {
        let data_dir = temp_data_dir("filters");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let mut first = record("n-1", "2026-07-03T10:00:00Z");
        first.session_id = Some(protocol::SessionId("s-1".to_owned()));
        store.append_created(first.clone()).expect("append first");
        let mut second = record("n-2", "2026-07-03T11:00:00Z");
        second.status = NotificationStatus::Archived;
        second.kind = NotificationKind::Error;
        second.severity = NotificationSeverity::Error;
        second.source.provider = "claude".to_owned();
        second.session_id = Some(protocol::SessionId("s-2".to_owned()));
        store.append_created(second).expect("append second");

        let listed = store
            .list(protocol::NotificationListParams {
                status: Some(NotificationStatus::Unread),
                kind: Some(NotificationKind::ApprovalRequired),
                severity: Some(NotificationSeverity::ActionRequired),
                provider: Some("codex".to_owned()),
                session_id: Some(protocol::SessionId("s-1".to_owned())),
                created_after: Some("2026-07-03T09:00:00Z".to_owned()),
                created_before: Some("2026-07-03T10:30:00Z".to_owned()),
                limit: None,
                cursor: None,
            })
            .expect("list");

        assert_eq!(listed.notifications, vec![first]);
        assert_eq!(listed.next_cursor, None);
    }

    #[test]
    fn list_cursor_orders_by_created_at_desc_then_id() {
        let data_dir = temp_data_dir("cursor");
        let store = NotificationStore::open(&data_dir).expect("open store");
        for id in ["n-2", "n-1", "n-3"] {
            store
                .append_created(record(id, "2026-07-03T10:00:00Z"))
                .expect("append");
        }
        store
            .append_created(record("n-4", "2026-07-03T11:00:00Z"))
            .expect("append newer");

        let first_page = store
            .list(protocol::NotificationListParams {
                limit: Some(2),
                ..protocol::NotificationListParams::default()
            })
            .expect("first page");
        let second_page = store
            .list(protocol::NotificationListParams {
                limit: Some(10),
                cursor: first_page.next_cursor.clone(),
                ..protocol::NotificationListParams::default()
            })
            .expect("second page");

        let first_ids: Vec<_> = first_page
            .notifications
            .iter()
            .map(|record| record.id.0.as_str())
            .collect();
        let second_ids: Vec<_> = second_page
            .notifications
            .iter()
            .map(|record| record.id.0.as_str())
            .collect();
        assert_eq!(first_ids, vec!["n-4", "n-1"]);
        assert_eq!(second_ids, vec!["n-2", "n-3"]);
    }

    #[test]
    fn list_default_excludes_deleted_unless_status_filter_requests_deleted() {
        let data_dir = temp_data_dir("list-deleted");
        let store = NotificationStore::open(&data_dir).expect("open store");
        let mut deleted = record("n-1", "2026-07-03T10:00:00Z");
        store
            .append_created(deleted.clone())
            .expect("append deleted record");
        deleted.status = NotificationStatus::Deleted;
        deleted.deleted_at = Some("2026-07-03T10:05:00Z".to_owned());
        store
            .append_deleted(deleted.clone())
            .expect("append delete action");
        let visible = record("n-2", "2026-07-03T11:00:00Z");
        store
            .append_created(visible.clone())
            .expect("append visible record");

        let default_list = store
            .list(protocol::NotificationListParams::default())
            .expect("default list");
        let deleted_list = store
            .list(protocol::NotificationListParams {
                status: Some(NotificationStatus::Deleted),
                ..protocol::NotificationListParams::default()
            })
            .expect("deleted list");

        assert_eq!(default_list.notifications, vec![visible]);
        assert_eq!(deleted_list.notifications, vec![deleted]);
    }
}
