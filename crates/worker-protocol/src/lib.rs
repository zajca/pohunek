//! Private protocol for durable pohunek session workers.
//!
//! The daemon and each local session worker use this crate for their
//! owner-private Unix-socket protocol. Control messages use bounded
//! newline-delimited JSON. Terminal output and attach input use a binary-safe
//! framing layer whose JSON header and byte payload are bounded separately.
//!
//! This protocol is intentionally separate from the public pohunek protocol.
//! It is local-only, version-negotiated, and supports the current and immediately
//! preceding worker protocol versions.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-08-04

/// Maximum lifetime accepted for a worker-private identity claim.
///
/// This intentionally matches the public protocol ceiling so neither ingress
/// path can create a longer-lived claim.
pub const MAX_IDENTITY_CLAIM_TTL_SECS: u64 = 60;

mod codec;
mod control;
mod data;
mod id;
mod secret;
mod token;
mod version;

#[doc(inline)]
pub use codec::{ControlCodecError, ControlReader, ControlWriter, MAX_CONTROL_LINE_BYTES};
#[doc(inline)]
pub use control::{
    ActiveIdentityClaim, AttachStart, Capability, ControlCode, ControlError, ControlEvent,
    ControlMessage, ControlRequest, ControlResponse, ControlTypeError, Dimensions, EventKind,
    ExitStatus, Initialize, InitializeLimits, InputFragment, InputPlan, InspectSnapshot,
    LaunchIdentity, OutputGap, ProcessIdentity, ReleasedIdentityClaim, ReportedLaunchIdentity,
    RequestKind, ResizeRequest, ResponseKind, RuntimePhase, RuntimeScope, StopPolicy, StopRequest,
    StreamMode, WriteAck,
};
#[doc(inline)]
pub use data::{
    read_frame, write_frame, CloseReason, Cursor, DataFrame, FrameError, FrameHeader, FrameKind,
    TerminalSnapshot, MAX_DATA_HEADER_BYTES, MAX_DATA_PAYLOAD_BYTES,
};
#[doc(inline)]
pub use id::{
    DaemonId, IdError, LeaseId, RequestId, RuntimeId, SessionId, StreamId, TransactionId, WorkerId,
    WriteId,
};
#[doc(inline)]
pub use secret::{DataToken, LeaseChallenge, SecretBytes, SecretEnv, SecretError};
#[doc(inline)]
pub use token::{TokenClaims, TokenError, TokenVault};
#[doc(inline)]
pub use version::{
    negotiate, Version, VersionError, VersionRange, ATTACH_SNAPSHOT_VERSION,
    CONTROL_PLANE_OBSERVATION_VERSION, CURRENT_VERSION, PREVIOUS_VERSION, SUPPORTED_RANGE,
};
