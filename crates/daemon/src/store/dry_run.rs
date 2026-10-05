//! Read-only dry run of the startup migration.
//!
//! The dry run reads the store through the same functions
//! [`Store::migrate_to_current`] and every later load use, and keeps the
//! result in memory: it takes no write lock, makes no backup and never writes
//! the store. What it returns is what a daemon of this build would load after
//! its startup migration.

// Rust guideline compliant 2026-06-26

use std::collections::HashMap;
use std::io;

use serde_json::Value;

use super::legacy_binding::{self, RecoveryOutcome};
use super::schema::{
    classify, migrated_body, parse_lines, ParsedLine, SchemaState, LEGACY_BINDING_STEP_FROM,
};
use super::{check_body_size, partition_lines, RejectedLine, ResumeBinding, SessionRecord, Store};

/// What the startup migration would do to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DryRunState {
    /// There is no store file.
    Missing,
    /// Every record has the current schema.
    UpToDate,
    /// The store is rewritten at the current schema; `from` is the oldest
    /// schema found.
    WouldMigrate { from: u32 },
}

/// The store as the daemon would load it after its startup migration.
#[derive(Debug)]
pub(crate) struct DryRun {
    /// What the startup migration does.
    pub(crate) state: DryRunState,
    /// Records the migration carries over.
    pub(crate) records: usize,
    /// Every logical session record that loads.
    pub(crate) sessions: Vec<SessionRecord>,
    /// Every resume binding that loads.
    pub(crate) resume: Vec<ResumeBinding>,
    /// Every line the load skips.
    pub(crate) rejected: Vec<RejectedLine>,
    /// What the migration does to the native recovery of each session, keyed
    /// by session id; a session without an entry keeps its recovery.
    pub(crate) recovery: HashMap<String, RecoveryOutcome>,
}

impl Store {
    /// Migrates the store in memory and loads it, leaving the file untouched.
    ///
    /// # Errors
    ///
    /// Returns the errors [`Store::migrate_to_current`] returns before its
    /// first write: an [`io::Error`] wrapping a [`super::StoreSchemaError`]
    /// when the store is newer than this daemon or has no kept migration
    /// path, and an I/O error when it cannot be read.
    pub(crate) fn dry_run_migration(&self) -> io::Result<DryRun> {
        let Some(content) = self.read_content()? else {
            return Ok(DryRun {
                state: DryRunState::Missing,
                records: 0,
                sessions: Vec::new(),
                resume: Vec::new(),
                rejected: Vec::new(),
                recovery: HashMap::new(),
            });
        };
        let lines = parse_lines(&self.path, &content)?;
        let schema_state = classify(&self.path, &lines)?;
        // What the migration does to the recovery each line holds, by line.
        let mut assessed: Vec<Option<(String, RecoveryOutcome)>> = Vec::new();
        let (state, body) = match schema_state {
            SchemaState::Current => (DryRunState::UpToDate, None),
            SchemaState::Migratable { oldest } => {
                assessed = lines.iter().map(assess_line).collect();
                let (body, _records) = migrated_body(lines, content.len())?;
                check_body_size(&body)?;
                (DryRunState::WouldMigrate { from: oldest }, Some(body))
            }
        };
        let text = body.as_deref().unwrap_or(&content);
        let lines = parse_lines(&self.path, text)?;
        let records = lines
            .iter()
            .filter(|line| matches!(line, ParsedLine::Record { .. }))
            .count();
        let mut recovery = HashMap::new();
        let mut loaded = DryRun {
            state,
            records,
            sessions: Vec::new(),
            resume: Vec::new(),
            rejected: Vec::new(),
            recovery: HashMap::new(),
        };
        // Each line is loaded alone, so a line the loader skips contributes
        // nothing: startup keeps a session's embedded recovery when its
        // separate projection is skipped.
        for (index, line) in lines.into_iter().enumerate() {
            let ((resume, _worktrees, _projects, sessions), rejected) = partition_lines(vec![line]);
            if rejected.is_empty() {
                if let Some(Some((session_id, outcome))) = assessed.get(index) {
                    raise(&mut recovery, session_id, *outcome);
                }
            }
            loaded.resume.extend(resume);
            loaded.sessions.extend(sessions);
            loaded.rejected.extend(rejected);
        }
        for binding in loaded
            .sessions
            .iter()
            .filter_map(|session| session.recovery.as_ref())
            .chain(&loaded.resume)
            .filter(|binding| binding.native_launch_unresolved)
        {
            raise(
                &mut recovery,
                &binding.session_id,
                RecoveryOutcome::NeedsRegistry,
            );
        }
        loaded.recovery = recovery;
        Ok(loaded)
    }
}

/// What the legacy binding step does to the recovery `line` holds, with the
/// session it belongs to.
///
/// Only a line below the step's schema is rewritten by it, so a line at a
/// later schema is not assessed.
fn assess_line(line: &ParsedLine<'_>) -> Option<(String, RecoveryOutcome)> {
    let ParsedLine::Record { value, version, .. } = line else {
        return None;
    };
    if *version > LEGACY_BINDING_STEP_FROM {
        return None;
    }
    let (Value::Object(record), Some(session_id)) =
        (value, value.get("session_id").and_then(Value::as_str))
    else {
        return None;
    };
    Some((
        session_id.to_owned(),
        legacy_binding::assess_recovery(record),
    ))
}

/// Keeps the worst outcome seen for `session_id`.
fn raise(
    recovery: &mut HashMap<String, RecoveryOutcome>,
    session_id: &str,
    outcome: RecoveryOutcome,
) {
    let worst = recovery.entry(session_id.to_owned()).or_insert(outcome);
    *worst = (*worst).max(outcome);
}
