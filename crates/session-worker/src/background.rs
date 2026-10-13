//! Checks the background-work fields of a snapshot and builds the items the
//! worker journals.
//!
//! The Claude state hook sanitizes each field before it crosses the hook
//! boundary; the worker checks each one again, because the socket accepts any
//! local writer. A field that fails its check is dropped and counted, and the
//! rest of the snapshot is kept.

// Rust guideline compliant 2026-10-12

use std::collections::BTreeMap;

use pohunek_worker_protocol::{
    is_valid_agent_type, is_valid_label, is_valid_program, is_valid_provider_kind, BackgroundItem,
    BackgroundKind, BackgroundSnapshot, BackgroundStatus,
};
use serde_json::Value;

use crate::journal::JournalRecord;

/// Provider type labels with a closed kind, in Claude Code's task-type map.
const KNOWN_KINDS: &[(&str, BackgroundKind)] = &[
    ("subagent", BackgroundKind::Subagent),
    ("workflow", BackgroundKind::Workflow),
    ("shell", BackgroundKind::Shell),
    ("monitor", BackgroundKind::Monitor),
    ("MCP task", BackgroundKind::McpTask),
    ("teammate", BackgroundKind::Teammate),
    ("dream", BackgroundKind::Dream),
    ("auto-mode scan", BackgroundKind::AutoModeScan),
    ("memory import", BackgroundKind::MemoryImport),
    ("cloud session", BackgroundKind::CloudSession),
];

/// Maps a provider type label to its closed kind.
///
/// The label is kept only when the kind is [`BackgroundKind::Other`].
fn kind_of(provider_type: &str) -> (BackgroundKind, Option<String>) {
    KNOWN_KINDS
        .iter()
        .find(|(label, _)| *label == provider_type)
        .map_or_else(
            || (BackgroundKind::Other, Some(provider_type.to_owned())),
            |&(_, kind)| (kind, None),
        )
}

/// The closed status of a provider status label.
fn status_of(provider_status: &str) -> BackgroundStatus {
    match provider_status {
        "running" => BackgroundStatus::Running,
        "pending" => BackgroundStatus::Pending,
        _ => BackgroundStatus::Other,
    }
}

/// Counts of the optional fields a snapshot dropped, by reason code.
///
/// Only counts leave the worker, never the values.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dropped {
    /// Fields whose value failed its check, or had the wrong JSON type.
    pub(crate) invalid: usize,
    /// Fields the task's kind does not carry, such as a program on a subagent.
    pub(crate) kind_mismatch: usize,
}

impl Dropped {
    /// Whether no field was dropped.
    pub(crate) const fn is_empty(self) -> bool {
        self.invalid == 0 && self.kind_mismatch == 0
    }
}

/// One snapshot task with its background fields checked.
///
/// The optional fields are the raw JSON values the hook sent, so a wrong-typed
/// value is counted rather than failing the whole snapshot.
pub(crate) struct Candidate {
    id: String,
    kind: BackgroundKind,
    provider_kind: Option<String>,
    status: BackgroundStatus,
    agent_type: Option<String>,
    label: Option<String>,
    program: Option<String>,
}

/// Checks one optional text field and returns the text when it is valid.
///
/// Absent JSON values are absent. A value that is not a string, or fails
/// `accept`, is counted as invalid.
fn checked_text(
    value: Option<&Value>,
    accept: impl FnOnce(&str) -> bool,
    dropped: &mut Dropped,
) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if accept(text.as_str()) => Some(text.clone()),
        Some(_) => {
            dropped.invalid += 1;
            None
        }
    }
}

impl Candidate {
    /// Checks the background fields of one provider task.
    ///
    /// A field the kind does not carry is counted as a kind mismatch; a field
    /// of the right kind that fails its check is counted as invalid.
    pub(crate) fn new(
        id: &str,
        task_type: &str,
        status: &str,
        label: Option<&Value>,
        program: Option<&Value>,
        agent_type: Option<&Value>,
        dropped: &mut Dropped,
    ) -> Self {
        let (kind, provider_kind) = kind_of(task_type);
        let provider_kind = provider_kind.and_then(|label| {
            if is_valid_provider_kind(&label) {
                Some(label)
            } else {
                dropped.invalid += 1;
                None
            }
        });
        let label = checked_text(label, is_valid_label, dropped);
        let program = if kind == BackgroundKind::Shell {
            checked_text(program, is_valid_program, dropped)
        } else {
            dropped.kind_mismatch += usize::from(program.is_some_and(|value| !value.is_null()));
            None
        };
        let agent_type = if kind == BackgroundKind::Subagent {
            checked_text(agent_type, is_valid_agent_type, dropped)
        } else {
            dropped.kind_mismatch += usize::from(agent_type.is_some_and(|value| !value.is_null()));
            None
        };
        Self {
            id: id.to_owned(),
            kind,
            provider_kind,
            status: status_of(status),
            agent_type,
            label,
            program,
        }
    }
}

/// Builds the items of a new snapshot observed at `observed_at_ms`.
///
/// An item keeps the `first_seen_ms` of the previous snapshot's item with the
/// same id and kind; any other item was first seen now.
pub(crate) fn build_items(
    candidates: Vec<Candidate>,
    previous: Option<&BackgroundSnapshot>,
    observed_at_ms: u64,
) -> Vec<BackgroundItem> {
    candidates
        .into_iter()
        .map(|candidate| {
            let first_seen_ms = previous
                .and_then(|snapshot| {
                    snapshot
                        .items
                        .iter()
                        .find(|item| item.id == candidate.id && item.kind == candidate.kind)
                })
                .map_or(observed_at_ms, |item| item.first_seen_ms);
            BackgroundItem {
                id: candidate.id,
                kind: candidate.kind,
                provider_kind: candidate.provider_kind,
                status: candidate.status,
                agent_type: candidate.agent_type,
                label: candidate.label,
                program: candidate.program,
                first_seen_ms,
                last_seen_ms: observed_at_ms,
            }
        })
        .collect()
}

/// The stable name of `kind`, as it appears in logs.
const fn kind_name(kind: BackgroundKind) -> &'static str {
    match kind {
        BackgroundKind::Subagent => "subagent",
        BackgroundKind::Workflow => "workflow",
        BackgroundKind::Shell => "shell",
        BackgroundKind::Monitor => "monitor",
        BackgroundKind::McpTask => "mcp_task",
        BackgroundKind::Teammate => "teammate",
        BackgroundKind::Dream => "dream",
        BackgroundKind::AutoModeScan => "auto_mode_scan",
        BackgroundKind::MemoryImport => "memory_import",
        BackgroundKind::CloudSession => "cloud_session",
        BackgroundKind::Other => "other",
    }
}

/// Formats the item count per kind as `kind=count` pairs.
///
/// The pairs follow kind-name order. Counts only: no id, label or program.
pub(crate) fn kind_summary(items: &[BackgroundItem]) -> String {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for item in items {
        *counts.entry(kind_name(item.kind)).or_default() += 1;
    }
    counts
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drops the background snapshot of `journal` and reports whether one was held.
///
/// The owner goes with the snapshot. The revision counter is bumped only when
/// a snapshot was dropped. The sequence mark is not lowered, so an older
/// snapshot delivered later is still stale. Runtime end clears this way: its
/// ordering needs no sequence, because the runtime's claims end with it.
pub(crate) fn clear(journal: &mut JournalRecord) -> bool {
    journal.background_owner = None;
    if journal.background.take().is_none() {
        return false;
    }
    journal.background_revision = journal.background_revision.saturating_add(1);
    true
}

/// Clears the background like [`clear`], and raises the sequence mark to `at`.
///
/// A clear ordered at `at` makes every claim ordered before it stale, so a
/// hook that began before the clear and delivers late cannot restore the
/// snapshot it was ordered before.
pub(crate) fn clear_at(journal: &mut JournalRecord, at: u64) -> bool {
    journal.background_sequence = journal.background_sequence.max(clamp_mark(at));
    clear(journal)
}

/// Nanoseconds in one second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Caps a claim sequence at the worker's own `CLOCK_MONOTONIC` reading.
///
/// The background mark is written with the clamped value, never with a claim's
/// raw sequence. A hook takes its sequence from a monotonic clock, but a
/// runtime package may keep a clock of its own, and its readings can run ahead
/// of the worker's. Without the clamp, one such value would make every later
/// genuine claim stale until the runtime ends. With it, the mark is never above
/// the clock, so the next genuine claim, ordered after the forged one arrived,
/// is newer and replaces the snapshot. Comparisons always use the unclamped
/// claim sequence.
pub(crate) fn clamp_mark(sequence: u64) -> u64 {
    sequence.min(monotonic_nanos())
}

/// Reads the worker's `CLOCK_MONOTONIC` in nanoseconds, the clock the hooks take
/// their sequences from through `time.clock_gettime_ns(time.CLOCK_MONOTONIC)`.
/// The reading saturates instead of failing on an unrepresentable value.
pub(crate) fn monotonic_nanos() -> u64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(time.tv_sec)
        .unwrap_or(0)
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(u64::try_from(time.tv_nsec).unwrap_or(0))
}
