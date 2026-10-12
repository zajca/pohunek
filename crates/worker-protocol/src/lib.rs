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

/// Schema version of the worker's machine-readable capability response.
pub const WORKER_DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;

/// Maximum lifetime accepted for a worker-private identity claim.
///
/// This intentionally matches the public protocol ceiling so neither ingress
/// path can create a longer-lived claim.
pub const MAX_IDENTITY_CLAIM_TTL_SECS: u64 = 60;

mod background_fields;
mod codec;
mod control;
mod data;
mod env;
mod hook_schema;
mod id;
mod secret;
mod text;
mod token;
mod version;

#[doc(inline)]
pub use background_fields::{
    is_task_text, is_valid_agent_type, is_valid_label, is_valid_program, is_valid_provider_kind,
    is_valid_task_id,
};
#[doc(inline)]
pub use codec::{ControlCodecError, ControlReader, ControlWriter, MAX_CONTROL_LINE_BYTES};
#[doc(inline)]
pub use control::{
    ActiveIdentityClaim, AttachStart, BackgroundItem, BackgroundKind, BackgroundSnapshot,
    BackgroundStatus, Capability, ControlCode, ControlError, ControlEvent, ControlMessage,
    ControlRequest, ControlResponse, ControlTypeError, Dimensions, EventKind, ExitStatus,
    Initialize, InitializeLimits, InputFragment, InputPlan, InspectSnapshot, LaunchIdentity,
    OutputGap, ProcessIdentity, ReleasedIdentityClaim, ReportedLaunchIdentity,
    ReportedNativeReference, RequestKind, ResizeRequest, ResponseKind, RuntimePhase, RuntimeScope,
    StopPolicy, StopRequest, StreamMode, SubagentPhase, SubagentSnapshot, WriteAck,
    BACKGROUND_AGENT_TYPE_MAX_BYTES, BACKGROUND_LABEL_MAX_BYTES, BACKGROUND_MAX_ITEMS,
    BACKGROUND_PROGRAM_MAX_BYTES, BACKGROUND_TASK_ID_MAX_BYTES, BACKGROUND_TASK_STATUS_MAX_BYTES,
    BACKGROUND_TASK_TYPE_MAX_BYTES,
};
#[doc(inline)]
pub use data::{
    read_frame, write_frame, CloseReason, Cursor, DataFrame, FrameError, FrameHeader, FrameKind,
    TerminalSnapshot, MAX_DATA_HEADER_BYTES, MAX_DATA_PAYLOAD_BYTES,
};
#[doc(inline)]
pub use env::{
    is_denylisted, validate_pattern, BaseEnv, EnvError, DEFAULT_ENVIRONMENT_ALLOWLIST,
    MAX_BASE_ENV_BYTES, MAX_BASE_ENV_ENTRIES, MAX_BASE_ENV_NAME_BYTES, MAX_BASE_ENV_VALUE_BYTES,
    SERVICE_MANAGER_DENYLIST,
};
#[doc(inline)]
pub use hook_schema::{
    admit_hook, hook_schema, hook_schema_for_handler, hook_schemas, is_known_hook_handler,
    AncestryMatcher, HookAction, HookDenial, HookSchema, LaunchProcess, PtyRelation, ReferenceKind,
    SubagentField, SubagentSequence,
};
#[doc(inline)]
pub use id::{
    DaemonId, IdError, LeaseId, RequestId, SessionId, StreamId, TransactionId, WorkerId,
    WorkerInstanceId, WriteId,
};
#[doc(inline)]
pub use secret::{DataToken, LeaseChallenge, SecretBytes, SecretEnv, SecretError};
#[doc(inline)]
pub use text::is_hidden_or_separator;
pub use token::{TokenClaims, TokenError, TokenVault};
#[doc(inline)]
pub use version::{
    negotiate, Version, VersionError, VersionRange, ATTACH_SNAPSHOT_VERSION,
    BASE_ENVIRONMENT_VERSION, CONTROL_PLANE_OBSERVATION_VERSION, CURRENT_VERSION, PREVIOUS_VERSION,
    SUBAGENT_OBSERVATION_VERSION, SUPPORTED_RANGE,
};
