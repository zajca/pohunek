//! Child-process primitives shared by the CLI's runners.
//!
//! Runners own their policy (program, argv, environment, deadline, and output
//! limits); this module owns the audited process lifecycle beneath it.

// Rust guideline compliant 2026-10-13

pub(crate) mod bounded;
