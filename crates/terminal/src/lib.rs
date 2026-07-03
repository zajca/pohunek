//! Terminal screen modeling shared by the pohunek daemon and CLI.
//!
//! Two `vt100`-backed concerns live here so the daemon and CLI share one screen
//! model instead of drifting copies:
//!
//! - [`ScreenTracker`] scrapes visible text and agent prompt/rule structure from
//!   a PTY byte stream; the daemon uses it for activity detection.
//! - [`Compositor`] re-renders the physical terminal for `pohunek attach`,
//!   reserving a banner row above the live agent grid so a status banner works
//!   even under full-screen TUI agents such as Codex and Claude Code.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-06-26

mod compositor;
mod screen;

pub use compositor::{Compositor, MIN_ROWS_WITH_BANNER};
pub use screen::{ScreenRegion, ScreenTracker};
