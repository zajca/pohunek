//! Owner-private durable host-state filesystem primitives.
//!
//! This module owns path-safe record I/O and the host-state process lock. Record
//! schemas remain outside this layer so every future host-state format shares the
//! same no-follow, owner-only, crash-safe implementation.

// Rust guideline compliant 2026-09-03

mod approval_key;
mod persistence;
pub(crate) mod records;
mod repository;

#[doc(inline)]
pub use persistence::{HostStateDir, HostStateError, HostStateLock, MAX_RECORD_BYTES};
#[doc(inline)]
pub use repository::{HostStateRepository, HostStateRepositoryError, HostStateSnapshot};
