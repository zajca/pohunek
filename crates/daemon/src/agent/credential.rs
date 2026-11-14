//! Detects credential literals in profile inputs.
//!
//! The detector lives in [`package::credential`] so the CLI applies the same
//! checks before it prints a proposed profile; the daemon's call sites use it
//! through this module.

// Rust guideline compliant 2026-11-14

pub(crate) use package::credential::{
    credential_arg_problem, credential_literal_problem, credential_name_problem,
};
