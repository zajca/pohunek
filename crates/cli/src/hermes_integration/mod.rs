//! Hermes operator-plugin integration primitives.
//!
//! This internal module owns the fail-closed Hermes plugin lifecycle.

// Rust guideline compliant 2026-08-06

pub(crate) mod assets;
pub(crate) mod doctor;
pub(crate) mod error;
pub(crate) mod lifecycle;
pub(crate) mod policy;
pub(crate) mod runner;
pub(crate) mod skill;
pub(crate) mod target;
