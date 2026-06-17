//! TODO(phase-1 milestone 3): session model + supervisor.
//!
//! Per `docs/plan-phase-1.md` "Build Order" step 3 and `docs/architecture.md`
//! "Concurrency and supervision": each session is isolated so a crashing session
//! cannot take down the daemon. Binds host/repo/worktree/branch/agent/resume
//! metadata. Not implemented in milestones 1-2.
