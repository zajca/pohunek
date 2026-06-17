//! TODO(phase-1 milestone 3): PTY actor (thread-per-PTY, resize, reader bridge).
//!
//! Per `docs/plan-phase-1.md` "PTY Ownership (Actor Model)" and "Build Order"
//! step 3: one dedicated OS thread per PTY does blocking 8 KB reads via
//! `portable-pty`, bridged to async; a `Handle` exposes `write_user_input`,
//! `resize`, and `shutdown`. Not implemented in milestones 1-2.
