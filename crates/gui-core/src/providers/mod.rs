//! Provider integrations for the native GUI core.

// Rust guideline compliant 2026-06-26

/// Compatibility field shared by provider prompt JSON payloads.
pub const COMPAT_BRANCH_FIELD: &str = "branch";

pub mod filters;
pub mod github;
pub mod linear;
