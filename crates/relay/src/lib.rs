//! Optional team relay process and service composition.

#![forbid(unsafe_code)]
#![forbid(clippy::disallowed_types, clippy::disallowed_methods)]

// Rust guideline compliant 2026-09-08

pub mod admission;
pub mod auth;
pub mod authorization;
pub mod config;
pub mod lifecycle;
pub mod operator;
pub mod recovery;
pub mod runtime;
pub mod server;
pub mod store;
