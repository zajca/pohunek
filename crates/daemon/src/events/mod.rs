//! TODO(phase-1 milestone 9): append-only event log.
//!
//! Per `docs/plan-phase-1.md` "SQLite Schema" (event log note) and
//! `docs/architecture.md` "Configuration, State, and Log Storage": a local
//! append-only JSON-lines log under `events/`, the audit/debug trail, never
//! storing secrets. `state.db` is rebuildable; this log is not. Not implemented
//! in milestones 1-2.
