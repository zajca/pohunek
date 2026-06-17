//! zagentmesh control protocol.
//!
//! This crate defines the typed control envelopes exchanged between the CLI and
//! the daemon over the local Unix socket (and, in Phase 2, over NetBird TCP).
//! The wire format is newline-delimited JSON: exactly one JSON value per line
//! (see `docs/plan-phase-1.md` "Control Protocol" and `docs/architecture.md`
//! "Transport and Control Protocol").
//!
//! It is deliberately shared so the CLI and daemon cannot drift, and so Phase 2's
//! NetBird transport reuses it unchanged.
//!
//! Design rules carried from the plan:
//! - Every envelope carries `v` (protocol version). New fields are additive and
//!   unknown fields are ignored, so a newer peer and an older peer interoperate
//!   on the common subset.
//! - Requests carry an `id` correlating the response and any related events.
//! - Errors are typed (class + machine code + human message + optional recovery
//!   hint) so `--json` consumers and operator agents can branch on them.

#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
#![forbid(unsafe_code)]

mod envelope;
mod error;
mod session;
mod version;

pub use envelope::{Event, Request, Response, StateSource};
pub use error::{ErrorClass, ProtocolError};
pub use session::{
    AgentKind, AttachHeader, SessionAttachParams, SessionAttachResult, SessionDetachParams,
    SessionDetachResult, SessionId, SessionInfo, SessionNewParams, SessionResizeParams,
    SessionResizeResult, SessionState, SessionStopResult,
};
pub use version::{negotiate, ProtocolVersion, PROTOCOL_VERSION};

/// Control-protocol method names (Phase 1).
///
/// These are the `method` values a request may carry. They are kept as
/// constants rather than an enum because the wire field is an open string: an
/// older daemon must be able to receive a method it does not know and answer
/// with a typed `method_not_found` error instead of failing to deserialize.
///
/// See `docs/plan-phase-1.md` "Control Protocol" (Methods, Phase 1). Only
/// `daemon.health` is handled by the daemon in milestone 2; the rest are
/// declared here so the contract is stable as later milestones land.
pub mod method {
    /// Liveness/version probe. Implemented in milestone 2.
    pub const DAEMON_HEALTH: &str = "daemon.health";

    // --- Declared for later milestones (not yet handled by the daemon). ---
    pub const SESSION_NEW: &str = "session.new";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_INSPECT: &str = "session.inspect";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_ATTACH: &str = "session.attach";
    pub const SESSION_DETACH: &str = "session.detach";
    pub const SESSION_RESIZE: &str = "session.resize";
    pub const STATUS: &str = "status";
    pub const SUBSCRIBE: &str = "subscribe";
    /// Fire-and-forget native-session-id capture from the agent hook.
    pub const SESSION_REPORT_NATIVE_ID: &str = "session.report_native_id";
}

/// Control-protocol event names.
///
/// These are the `event` values published on subscription connections. The
/// payload remains an open JSON object at the envelope layer.
pub mod event {
    pub const ATTACH_OPENED: &str = "attach_opened";
    pub const ATTACH_CLOSED: &str = "attach_closed";
    pub const SESSION_CREATED: &str = "session_created";
    pub const SESSION_UPDATED: &str = "session_updated";
    pub const SESSION_STOPPED: &str = "session_stopped";
}
