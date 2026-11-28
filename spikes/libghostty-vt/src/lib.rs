//! Measurement spike: do the PTY traces Pohunek cares about replay the same
//! through its current screen model (`vt100`) and through libghostty-vt?
//!
//! The crate is a standalone workspace under `spikes/libghostty-vt/`. It is
//! never a member of the root workspace, never published and never linked into
//! a shipped binary; the default build has no Zig and no C library.
//!
//! - [`trace`]: the versioned PTY trace format.
//! - `record-trace` (binary): runs a command in a real PTY and writes a trace.
//! - [`expect`]: spec-derived assertions in a `*.expect.toml` sidecar.
//! - [`model`], [`vt100_model`], [`registry`]: the model interface, the shipped
//!   model, the same parser with scrollback, and the models by name.
//! - [`replay`], [`report`]: replay every fixture through every model and
//!   list each assertion outcome and each grid difference.
//! - `ghostty_model` (feature `libghostty`): the libghostty-vt model, driven
//!   through `pohunek-terminal`'s `with_model` seam and the FFI crate
//!   `crates/terminal-ghostty-sys` (issue #2538 P2).
//!
//! Record a trace: `cargo run --bin record-trace -- --help`; the scripts under
//! `fixtures/sources/` state their own arguments in a `# record:` line and
//! `fixtures/sources/record-all.sh` records all of them.

// Rust guideline compliant 2026-06-26

pub mod config;
pub mod expect;
pub mod model;
pub mod registry;
pub mod replay;
pub mod report;
pub mod trace;
pub mod vt100_model;

#[cfg(feature = "libghostty")]
pub mod ghostty_model;
