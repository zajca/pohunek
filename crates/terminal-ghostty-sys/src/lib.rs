//! Safe Rust bindings to the pinned libghostty-vt static library (issue #2538).
//!
//! This crate is the only place that touches the C ABI: `pohunek-terminal`
//! keeps `#![forbid(unsafe_code)]` and drives the safe [`HeadlessTerminal`]
//! wrapper through its screen-backend seam. The `libghostty` feature links
//! the prebuilt library named by `LIBGHOSTTY_VT_DIR` (built outside cargo by
//! `scripts/build-libghostty-vt` from `scripts/libghostty-vt.pin.toml`);
//! without the feature the crate is empty, so a default build needs no Zig
//! and no C library, and nothing is ever downloaded at build or test time.
//!
//! - `ffi`: the crate-private `extern "C"` declarations and `repr(C)`
//!   mirrors, checked at compile time against the ABI layout the build
//!   script recorded from a C probe; only the formatter extra masks are
//!   re-exported at the root, nothing outside the crate can call the raw
//!   items (issue #2593 finding 12).
//! - [`ghostty`]: the safe, headless terminal wrapper (`HeadlessTerminal`)
//!   and its error type.
//! - [`config`]: the headless bounds and settings the terminal is created
//!   with.

#[cfg(feature = "libghostty")]
pub mod config;
#[cfg(feature = "libghostty")]
pub(crate) mod ffi;
#[cfg(feature = "libghostty")]
pub mod ghostty;

#[cfg(feature = "libghostty")]
pub use config::HeadlessConfig;
#[cfg(feature = "libghostty")]
pub use ffi::{FORMATTER_EXTRAS_ALL, FORMATTER_EXTRAS_ATTACH};
#[cfg(feature = "libghostty")]
pub use ghostty::{version, CellInfo, GhosttyError, HeadlessTerminal};
