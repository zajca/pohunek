//! Headless settings of the libghostty-vt terminal (issue #2538).
//!
//! The worker parses untrusted PTY bytes with this library, so every bound is
//! applied explicitly and the library's own defaults are never relied on.
//! The values are the pinned 0.46 model settings: they come from the spike's
//! `spikes/libghostty-vt/config/model.toml` and are named constants here, so
//! this crate needs no config file and no dependency to read one.

// Rust guideline compliant 2026-11-27

/// Scrollback limit in bytes: 8 MiB.
///
/// Scrollback is kept in pages of about 400 KB, so the limit is rounded up to
/// a whole number of pages. The worker's snapshot has no scrollback view, so
/// this bounds only the memory one busy session can retain.
pub const SCROLLBACK_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Largest APC sequence kept, in bytes: 64 KiB.
///
/// Kitty graphics are compiled out of the pinned build, so an APC can only
/// grow this buffer.
pub const APC_MAX_BYTES: usize = 64 * 1024;

/// Largest kept unknown sequence, in bytes: 4 KiB.
pub const UNKNOWN_MAX_BYTES: usize = 4 * 1024;

/// Largest parser continuation, in bytes: 64 KiB.
pub const CONTINUATION_MAX_BYTES: usize = 64 * 1024;

/// Largest OSC 5522 clipboard write transaction, in bytes: 1 MiB.
///
/// The headless terminal answers every write "denied", so this bounds only
/// the decode work a program can ask for.
pub const CLIPBOARD_WRITE_MAX_BYTES: usize = 1024 * 1024;

/// Cell size in pixels reported on resize: 1.
///
/// Only size reports use it; the worker never renders, so the value is a
/// placeholder that keeps the report well-formed.
pub const CELL_PX: u32 = 1;

/// The headless bounds and settings a [`HeadlessTerminal`](crate::HeadlessTerminal)
/// is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessConfig {
    /// Scrollback limit in bytes; see [`SCROLLBACK_MAX_BYTES`].
    pub scrollback_max_bytes: usize,
    /// Largest APC sequence in bytes; see [`APC_MAX_BYTES`].
    pub apc_max_bytes: usize,
    /// Largest kept unknown sequence in bytes; see [`UNKNOWN_MAX_BYTES`].
    pub unknown_max_bytes: usize,
    /// Largest parser continuation in bytes; see [`CONTINUATION_MAX_BYTES`].
    pub continuation_max_bytes: usize,
    /// Largest OSC 5522 write transaction in bytes; see
    /// [`CLIPBOARD_WRITE_MAX_BYTES`].
    pub clipboard_write_max_bytes: usize,
    /// Cell size in pixels passed on resize; see [`CELL_PX`].
    pub cell_px: u32,
}

impl Default for HeadlessConfig {
    /// The pinned 0.46 model settings, as the named constants above.
    fn default() -> Self {
        Self {
            scrollback_max_bytes: SCROLLBACK_MAX_BYTES,
            apc_max_bytes: APC_MAX_BYTES,
            unknown_max_bytes: UNKNOWN_MAX_BYTES,
            continuation_max_bytes: CONTINUATION_MAX_BYTES,
            clipboard_write_max_bytes: CLIPBOARD_WRITE_MAX_BYTES,
            cell_px: CELL_PX,
        }
    }
}
