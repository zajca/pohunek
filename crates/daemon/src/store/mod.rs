//! TODO(phase-1 milestone 9): SQLite store + migrations.
//!
//! Per `docs/plan-phase-1.md` "SQLite Schema" and "Build Order" step 9: a single
//! local `state.db` via `rusqlite` (bundled) with versioned `user_version`
//! migrations. Holds session and worktree metadata and resume bindings. Not
//! implemented in milestones 1-2 (the plan defers the store explicitly).
