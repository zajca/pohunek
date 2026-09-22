//! Defines narrow operating-system contracts for Pohunek.
//!
//! This crate owns process facts and kernel peer identity shared by the daemon
//! and session worker. It deliberately excludes session policy, provider
//! interpretation, public wire types, GUI state, and supervisor policy.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-09-19

#[cfg(unix)]
pub mod filesystem;
pub mod peer;
pub mod process;
pub mod supervisor;
