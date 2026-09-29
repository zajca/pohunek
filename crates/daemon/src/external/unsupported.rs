//! Placeholder backend for targets without a live transcript watcher.
//!
//! Opening it always fails, so the observer reports the watcher unavailable and
//! relies on the startup scan and the process sweep.

// Rust guideline compliant 2026-09-29

use std::io;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use super::watch::{RootRegistration, WatchBackend, WatchEvent};

/// Cause reported when the target has no live transcript watcher.
pub(super) const OPEN_FAILED_CAUSE: &str = "unsupported_target";

/// Backend that cannot be opened on this target.
#[derive(Debug)]
pub(super) struct UnsupportedBackend;

impl UnsupportedBackend {
    /// Always fails with [`io::ErrorKind::Unsupported`].
    pub(super) fn open() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live transcript watching is not implemented for this target",
        ))
    }
}

impl WatchBackend for UnsupportedBackend {
    fn register_root(&mut self, _root: &Path, _cancel: &CancellationToken) -> RootRegistration {
        RootRegistration::Missing
    }

    fn restart(&mut self) -> io::Result<()> {
        Self::open().map(|_backend| ())
    }

    fn unregister_all(&mut self) {}

    async fn next_events(&mut self, _out: &mut Vec<WatchEvent>) -> io::Result<()> {
        std::future::pending().await
    }
}
