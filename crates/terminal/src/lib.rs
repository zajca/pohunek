//! Terminal screen modeling shared by the pohunek daemon and CLI.
//!
//! Two `vt100`-backed concerns live here so the daemon and CLI share one screen
//! model instead of drifting copies:
//!
//! - [`ScreenTracker`] scrapes visible text and agent prompt/rule structure from
//!   a PTY byte stream; the daemon uses it for activity detection.
//! - [`Compositor`] shadows raw attach output and temporarily draws the session
//!   menu without sacrificing native terminal scrollback between menu opens.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-07-22

mod compositor;
mod menu;
mod screen;
mod snapshot;

#[doc(inline)]
pub use compositor::{Compositor, OverlayFrame, OverlayLine, BANNER_ROWS, MIN_ROWS_WITH_BANNER};
#[doc(inline)]
pub use menu::{step, MenuEffect, MenuEvent, MenuKey, MenuOutcome, MenuState};
pub use screen::{ScreenRegion, ScreenTracker};
pub use snapshot::{TerminalSnapshot, TerminalTracker};
pub use vt100::{MouseProtocolEncoding, MouseProtocolMode};
