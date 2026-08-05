//! Typed worker control messages.
//!
//! Requests, responses, and asynchronous events share one bounded NDJSON
//! connection. Unknown additive object fields are ignored by serde, while
//! unknown operations fail deserialization and affect only that connection.

// Rust guideline compliant 2026-08-04

use std::fmt::{Debug, Formatter};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    DaemonId, DataToken, LeaseChallenge, LeaseId, RequestId, RuntimeId, SecretBytes, SecretEnv,
    SessionId, StreamId, TransactionId, Version, VersionRange, WorkerId, WriteId,
};

/// Features a daemon or worker may negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Atomic output replay followed by live subscription.
    AtomicReplay,
    /// Structured terminal snapshots paired with ANSI repaint bytes.
    TerminalSnapshot,
    /// Deduplicated ordered PTY input plans.
    DeduplicatedInput,
    /// Worker-local agent identity hook reports.
    IdentityHook,
    /// Atomic initial resize, terminal repaint, and live attach subscription.
    AttachSnapshot,
    /// One-shot, runtime-bound terminal and retained-output observation.
    ControlPlaneObservation,
}

/// Describes the worker runtime lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    /// The worker has not received initialization.
    Uninitialized,
    /// The worker is allocating the PTY and spawning the child.
    Starting,
    /// The PTY and child process group are live.
    Running,
    /// An explicit stop is terminating the process group.
    Stopping,
    /// The child reached a recorded terminal outcome.
    Exited,
    /// The worker encountered a runtime fault.
    Faulted,
}

/// Identifies one operating-system process without trusting PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    /// Operating-system process ID.
    pub pid: u32,
    /// Platform process-start identity used to detect PID reuse.
    pub start_identity: u64,
}

/// Defines a validated terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Dimensions {
    columns: u16,
    rows: u16,
}

impl Dimensions {
    /// Creates nonzero terminal dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`ControlTypeError::InvalidDimensions`] when either value is
    /// zero.
    pub const fn new(columns: u16, rows: u16) -> Result<Self, ControlTypeError> {
        if columns == 0 || rows == 0 {
            Err(ControlTypeError::InvalidDimensions { columns, rows })
        } else {
            Ok(Self { columns, rows })
        }
    }

    /// Returns the terminal column count.
    #[must_use]
    pub const fn columns(self) -> u16 {
        self.columns
    }

    /// Returns the terminal row count.
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows
    }
}

impl<'de> Deserialize<'de> for Dimensions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireDimensions {
            columns: u16,
            rows: u16,
        }

        let dimensions = WireDimensions::deserialize(deserializer)?;
        Self::new(dimensions.columns, dimensions.rows).map_err(serde::de::Error::custom)
    }
}

/// Reports invalid strongly typed control values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ControlTypeError {
    /// Terminal dimensions contained a zero axis.
    #[error("terminal dimensions must be nonzero, got {columns}x{rows}")]
    InvalidDimensions {
        /// Requested columns.
        columns: u16,
        /// Requested rows.
        rows: u16,
    },
    /// An initialization memory or retention limit was zero.
    #[error("initialize limits must all be nonzero")]
    InvalidLimits,
    /// A stop grace period was zero.
    #[error("stop grace period must be nonzero")]
    InvalidStopPolicy,
    /// An input plan contained no fragments or only empty fragments.
    #[error("input plan must contain at least one nonempty fragment")]
    EmptyInputPlan,
}

/// Identifies the logical launch agent without provider credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchIdentity {
    /// Resolved agent profile name.
    pub agent: String,
    /// Stable provider base, such as `codex` or `claude`.
    pub agent_base: String,
    /// Frozen native recovery reference kind (`id` or `path`), when resumable.
    pub reference_kind: Option<String>,
}

/// Immutable provider-native recovery identity accepted from the launch hook.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedLaunchIdentity {
    /// Stable provider base.
    pub provider: String,
    /// Designated launch process identity.
    pub process: ProcessIdentity,
    /// Frozen native recovery reference kind.
    pub reference_kind: String,
    /// Provider-native recovery reference.
    pub native_reference: String,
}

impl Debug for ReportedLaunchIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportedLaunchIdentity")
            .field("provider", &self.provider)
            .field("process", &self.process)
            .field("reference_kind", &self.reference_kind)
            .field("native_reference", &"[REDACTED]")
            .finish()
    }
}

/// Latest active provider claim recorded by the worker.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveIdentityClaim {
    /// Stable provider base.
    pub provider: String,
    /// Claiming process identity.
    pub process: ProcessIdentity,
    /// Monotonic hook sequence.
    pub sequence: u64,
    /// RFC 3339 claim expiry.
    pub expires_at: String,
    /// Native-reference kind reported by the active provider.
    pub reference_kind: Option<String>,
    /// Native reference for the active provider.
    pub native_reference: Option<String>,
}

impl Debug for ActiveIdentityClaim {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveIdentityClaim")
            .field("provider", &self.provider)
            .field("process", &self.process)
            .field("sequence", &self.sequence)
            .field("expires_at", &self.expires_at)
            .field("reference_kind", &self.reference_kind)
            .field("native_reference", &"[REDACTED]")
            .finish()
    }
}

/// Ordering tombstone for an explicit worker-private identity release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedIdentityClaim {
    /// Provider base released by the hook.
    pub provider: String,
    /// Released process identity.
    pub process: ProcessIdentity,
    /// Monotonic release sequence.
    pub sequence: u64,
}

/// Defines bounded worker memory and retention limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct InitializeLimits {
    /// Maximum retained raw output bytes.
    output_history_bytes: u64,
    /// Maximum queued bytes for one output subscriber.
    subscriber_queue_bytes: u64,
    /// Maximum retained input deduplication entries.
    write_dedup_entries: u32,
    /// Milliseconds to retain terminal state awaiting daemon acknowledgement.
    terminal_retention_ms: u64,
}

impl InitializeLimits {
    /// Creates nonzero initialization limits.
    ///
    /// # Errors
    ///
    /// Returns [`ControlTypeError::InvalidLimits`] when any value is zero.
    pub const fn new(
        output_history_bytes: u64,
        subscriber_queue_bytes: u64,
        write_dedup_entries: u32,
        terminal_retention_ms: u64,
    ) -> Result<Self, ControlTypeError> {
        if output_history_bytes == 0
            || subscriber_queue_bytes == 0
            || write_dedup_entries == 0
            || terminal_retention_ms == 0
        {
            Err(ControlTypeError::InvalidLimits)
        } else {
            Ok(Self {
                output_history_bytes,
                subscriber_queue_bytes,
                write_dedup_entries,
                terminal_retention_ms,
            })
        }
    }

    /// Returns the retained raw output byte limit.
    #[must_use]
    pub const fn output_history_bytes(self) -> u64 {
        self.output_history_bytes
    }

    /// Returns the per-subscriber queued byte limit.
    #[must_use]
    pub const fn subscriber_queue_bytes(self) -> u64 {
        self.subscriber_queue_bytes
    }

    /// Returns the retained write-deduplication entry limit.
    #[must_use]
    pub const fn write_dedup_entries(self) -> u32 {
        self.write_dedup_entries
    }

    /// Returns terminal retention in milliseconds.
    #[must_use]
    pub const fn terminal_retention_ms(self) -> u64 {
        self.terminal_retention_ms
    }
}

impl<'de> Deserialize<'de> for InitializeLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireLimits {
            output_history_bytes: u64,
            subscriber_queue_bytes: u64,
            write_dedup_entries: u32,
            terminal_retention_ms: u64,
        }

        let limits = WireLimits::deserialize(deserializer)?;
        Self::new(
            limits.output_history_bytes,
            limits.subscriber_queue_bytes,
            limits.write_dedup_entries,
            limits.terminal_retention_ms,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Defines explicit process-group stop behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StopPolicy {
    /// Grace period between termination and forced kill.
    grace_ms: u64,
}

impl StopPolicy {
    /// Creates a nonzero stop grace period.
    ///
    /// # Errors
    ///
    /// Returns [`ControlTypeError::InvalidStopPolicy`] when `grace_ms` is zero.
    pub const fn new(grace_ms: u64) -> Result<Self, ControlTypeError> {
        if grace_ms == 0 {
            Err(ControlTypeError::InvalidStopPolicy)
        } else {
            Ok(Self { grace_ms })
        }
    }

    /// Returns the termination grace period in milliseconds.
    #[must_use]
    pub const fn grace_ms(self) -> u64 {
        self.grace_ms
    }
}

impl<'de> Deserialize<'de> for StopPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireStopPolicy {
            grace_ms: u64,
        }

        let policy = WireStopPolicy::deserialize(deserializer)?;
        Self::new(policy.grace_ms).map_err(serde::de::Error::custom)
    }
}

/// Supplies the one-shot worker launch plan.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Initialize {
    /// Durable logical session.
    pub session_id: SessionId,
    /// Idempotent create transaction.
    pub transaction_id: TransactionId,
    /// Worker expected by the daemon's logical transaction.
    pub expected_worker_id: WorkerId,
    /// Sanitized launch identity.
    pub launch: LaunchIdentity,
    /// Resolved executable path.
    pub executable: PathBuf,
    /// Resolved command arguments.
    pub arguments: Vec<String>,
    /// Child working directory.
    pub cwd: PathBuf,
    /// Initial PTY dimensions.
    pub dimensions: Dimensions,
    /// Profile environment passed only to child construction.
    pub environment: SecretEnv,
    /// Bounded worker memory and retention configuration.
    pub limits: InitializeLimits,
    /// Explicit process-group stop policy.
    pub stop_policy: StopPolicy,
    /// Worker-local identity-hook protocol version.
    pub hook_protocol_version: Version,
    /// Public daemon RPC protocol version the managed child's provider hooks
    /// must advertise (`POHUNEK_PROTOCOL_VERSION`). Distinct from
    /// `hook_protocol_version`, which negotiates this private worker
    /// protocol; this crate does not depend on the public `protocol` crate,
    /// so the daemon threads the value through as a plain integer.
    pub public_protocol_version: u32,
}

impl Debug for Initialize {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Initialize")
            .field("session_id", &self.session_id)
            .field("transaction_id", &self.transaction_id)
            .field("expected_worker_id", &self.expected_worker_id)
            .field("launch", &self.launch)
            .field("executable", &"<redacted>")
            .field("argument_count", &self.arguments.len())
            .field("cwd", &"<redacted>")
            .field("dimensions", &self.dimensions)
            .field("environment", &self.environment)
            .field("limits", &self.limits)
            .field("stop_policy", &self.stop_policy)
            .field("hook_protocol_version", &self.hook_protocol_version)
            .field("public_protocol_version", &self.public_protocol_version)
            .finish()
    }
}

/// Stores one ordered PTY input fragment.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFragment {
    /// Bytes written and flushed as one fragment.
    pub bytes: SecretBytes,
    /// Delay owned by the worker after this fragment.
    pub delay_after_ms: u64,
}

impl Debug for InputFragment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputFragment")
            .field("bytes", &self.bytes)
            .field("delay_after_ms", &self.delay_after_ms)
            .finish()
    }
}

/// Defines one deduplicated ordered PTY input operation.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct InputPlan {
    /// Runtime-unique idempotency key.
    write_id: WriteId,
    /// Ordered fragments executed by the worker.
    fragments: Vec<InputFragment>,
}

impl InputPlan {
    /// Creates a nonempty input plan.
    ///
    /// # Errors
    ///
    /// Returns [`ControlTypeError::EmptyInputPlan`] when no fragment contains
    /// bytes.
    pub fn new(write_id: WriteId, fragments: Vec<InputFragment>) -> Result<Self, ControlTypeError> {
        if fragments.is_empty() || fragments.iter().all(|fragment| fragment.bytes.is_empty()) {
            Err(ControlTypeError::EmptyInputPlan)
        } else {
            Ok(Self {
                write_id,
                fragments,
            })
        }
    }

    /// Returns the total number of input bytes.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.fragments
            .iter()
            .map(|fragment| fragment.bytes.len())
            .sum()
    }

    /// Borrows the runtime-unique idempotency key.
    #[must_use]
    pub fn write_id(&self) -> &WriteId {
        &self.write_id
    }

    /// Borrows ordered input fragments.
    #[must_use]
    pub fn fragments(&self) -> &[InputFragment] {
        &self.fragments
    }

    /// Consumes the plan into its idempotency key and fragments.
    #[must_use]
    pub fn into_parts(self) -> (WriteId, Vec<InputFragment>) {
        (self.write_id, self.fragments)
    }
}

impl Debug for InputPlan {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputPlan")
            .field("write_id", &self.write_id)
            .field("fragment_count", &self.fragments.len())
            .field("byte_count", &self.byte_len())
            .field("contents", &"<redacted>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for InputPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireInputPlan {
            write_id: WriteId,
            fragments: Vec<InputFragment>,
        }

        let plan = WireInputPlan::deserialize(deserializer)?;
        Self::new(plan.write_id, plan.fragments).map_err(serde::de::Error::custom)
    }
}

/// Scopes a mutation to exactly one leased runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeScope {
    /// Current controller lease.
    pub lease_id: LeaseId,
    /// Durable logical session.
    pub session_id: SessionId,
    /// Expected worker process.
    pub worker_id: WorkerId,
    /// Expected uninterrupted PTY runtime.
    pub runtime_id: RuntimeId,
}

/// Selects the purpose of a worker data stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamMode {
    /// Public raw terminal attachment.
    Attach,
    /// Daemon semantic detector feed.
    Detector,
    /// One bounded control-plane output observation.
    Observation,
}

/// Defines the atomic start state for a public terminal attachment.
///
/// The worker applies `dimensions`, when present, before creating the complete
/// terminal snapshot and subscribing the stream to subsequent PTY output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachStart {
    /// Initial client terminal dimensions, when the client can determine them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<Dimensions>,
}

/// Carries an ordered terminal resize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeRequest {
    /// Attachment or daemon control source.
    pub source_id: StreamId,
    /// Source-local monotonic sequence.
    pub sequence: u64,
    /// Requested terminal dimensions.
    pub dimensions: Dimensions,
}

/// Carries one idempotent explicit stop operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopRequest {
    /// Durable stop transaction.
    pub transaction_id: TransactionId,
}

/// Reports completion of a deduplicated input plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteAck {
    /// Completed input operation.
    pub write_id: WriteId,
    /// Total bytes written and flushed.
    pub bytes_written: u64,
}

/// Records a child process terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Conventional exit code when available.
    pub code: Option<i32>,
    /// Terminating signal number when available.
    pub signal: Option<i32>,
    /// Whether an explicit operator stop initiated termination.
    pub stopped_by_user: bool,
    /// Millisecond Unix timestamp recorded by the worker.
    pub exited_at_ms: u64,
}

/// Reports current worker and runtime authority facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectSnapshot {
    /// Durable logical session.
    pub session_id: SessionId,
    /// Worker process identity.
    pub worker_id: WorkerId,
    /// Runtime generation when initialized.
    pub runtime_id: Option<RuntimeId>,
    /// Current lifecycle phase.
    pub phase: RuntimePhase,
    /// Worker operating-system identity.
    pub worker_process: ProcessIdentity,
    /// Root child identity when initialized.
    pub child_process: Option<ProcessIdentity>,
    /// Current terminal dimensions when initialized.
    pub dimensions: Option<Dimensions>,
    /// First retained output byte.
    pub history_start_offset: u64,
    /// Offset after the last observed output byte.
    pub next_offset: u64,
    /// Final child outcome when terminal.
    pub exit: Option<ExitStatus>,
    /// Immutable launch recovery identity captured while the daemon was absent.
    pub launch_identity: Option<ReportedLaunchIdentity>,
    /// Latest active provider claim captured while the daemon was absent.
    pub active_identity: Option<ActiveIdentityClaim>,
    /// Explicit release tombstone, absent when no private release was accepted.
    #[serde(default)]
    pub active_identity_release: Option<ReleasedIdentityClaim>,
}

/// Defines one daemon-to-worker request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    /// Correlates the response.
    pub request_id: RequestId,
    /// Requested operation and its typed fields.
    #[serde(flatten)]
    pub kind: RequestKind,
}

/// Enumerates daemon-to-worker control operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestKind {
    /// Negotiates the highest shared protocol version.
    Negotiate {
        /// Connecting daemon instance.
        daemon_instance_id: DaemonId,
        /// Oldest daemon-supported version.
        minimum_version: Version,
        /// Newest daemon-supported version.
        maximum_version: Version,
    },
    /// Acquires the single controller lease.
    AcquireController {
        /// Connecting daemon instance.
        daemon_instance_id: DaemonId,
        /// Connection-bound challenge returned by negotiation.
        challenge: LeaseChallenge,
        /// Features the daemon intends to use.
        requested_capabilities: Vec<Capability>,
    },
    /// Returns an authoritative runtime snapshot.
    Inspect {
        /// Current controller lease.
        lease_id: LeaseId,
    },
    /// Supplies the one-shot launch plan.
    Initialize {
        /// Current controller lease.
        lease_id: LeaseId,
        /// Sensitive launch plan.
        initialize: Initialize,
    },
    /// Mints one short-lived data-stream token.
    OpenDataStream {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// Data stream to authorize.
        stream_id: StreamId,
        /// Stream purpose.
        mode: StreamMode,
        /// Last processed output offset, when reconnecting.
        after_offset: Option<u64>,
        /// Atomic terminal state requested by a fresh public attachment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach: Option<AttachStart>,
    },
    /// Returns the current rendered terminal without attaching a data stream.
    TerminalSnapshot {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
    },
    /// Returns one bounded retained-output page without attaching a data stream.
    ReadOutput {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// Unique one-shot observation stream.
        stream_id: StreamId,
        /// Output cursor. An omitted cursor requests the newest bounded tail.
        after_offset: Option<u64>,
        /// Maximum raw output bytes in this response.
        max_bytes: u32,
        /// Optional bounded wait when the cursor is at the current end.
        wait_ms: u64,
    },
    /// Executes one deduplicated input plan.
    WritePlan {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// Sensitive ordered input.
        plan: InputPlan,
    },
    /// Applies an idempotent ordered terminal resize.
    Resize {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// Resize request.
        resize: ResizeRequest,
    },
    /// Explicitly terminates the retained process group.
    Stop {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// Stop operation.
        stop: StopRequest,
    },
    /// Confirms daemon import of final state.
    AcknowledgeTerminal {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
    },
    /// Gracefully releases the controller connection.
    ReleaseController {
        /// Current controller lease.
        lease_id: LeaseId,
    },
}

/// Defines one worker-to-daemon response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponse {
    /// Correlates the request.
    pub request_id: RequestId,
    /// Typed result or failure.
    #[serde(flatten)]
    pub kind: ResponseKind,
}

/// Enumerates worker control response results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseKind {
    /// Reports successful version negotiation.
    Negotiated {
        /// Selected shared version.
        selected_version: Version,
        /// Worker's full supported range.
        supported_range: VersionRange,
        /// Durable logical session.
        session_id: SessionId,
        /// Worker process.
        worker_id: WorkerId,
        /// Runtime generation when initialized.
        runtime_id: Option<RuntimeId>,
        /// Worker operating-system identity.
        worker_process: ProcessIdentity,
        /// Current runtime phase.
        phase: RuntimePhase,
        /// Worker-supported capabilities.
        capabilities: Vec<Capability>,
        /// Connection-bound lease challenge.
        challenge: LeaseChallenge,
    },
    /// Reports successful controller acquisition.
    ControllerAcquired {
        /// New memory-only controller lease.
        lease_id: LeaseId,
        /// Mutually supported controller capabilities.
        capabilities: Vec<Capability>,
    },
    /// Returns an authoritative runtime snapshot.
    Inspected {
        /// Current runtime authority facts.
        snapshot: Box<InspectSnapshot>,
    },
    /// Reports successful or idempotently repeated initialization.
    Initialized {
        /// New uninterrupted runtime generation.
        runtime_id: RuntimeId,
        /// Root child process.
        child_process: ProcessIdentity,
    },
    /// Returns one one-use data-stream credential.
    DataStreamOpened {
        /// Redacted one-use credential.
        token: DataToken,
        /// Worker monotonic millisecond expiry.
        expires_at_ms: u64,
    },
    /// Returns a rendered terminal snapshot for one runtime generation.
    TerminalSnapshot {
        /// Runtime that produced the snapshot.
        runtime_id: RuntimeId,
        /// Current terminal state.
        snapshot: Box<crate::TerminalSnapshot>,
    },
    /// Opens one bounded, framed output observation stream.
    OutputReadOpened {
        /// Redacted one-use data-stream credential.
        token: DataToken,
        /// Worker monotonic millisecond expiry.
        expires_at_ms: u64,
    },
    /// Reports successful input completion.
    WriteCompleted {
        /// Deduplicated write acknowledgement.
        acknowledgement: WriteAck,
    },
    /// Confirms resize application or idempotent replay.
    Resized {
        /// Applied source-local sequence.
        sequence: u64,
        /// Effective PTY dimensions.
        dimensions: Dimensions,
    },
    /// Reports the idempotent stop result.
    Stopped {
        /// Final outcome when already observed.
        exit: Option<ExitStatus>,
    },
    /// Confirms terminal acknowledgement persistence.
    TerminalAcknowledged,
    /// Confirms explicit controller release.
    ControllerReleased,
    /// Reports a typed request failure.
    Error {
        /// Structured failure.
        error: ControlError,
    },
}

/// Identifies a machine-readable control failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlCode {
    /// Peers have no common protocol version.
    WorkerProtocolIncompatible,
    /// Another live daemon owns the controller lease.
    ControllerBusy,
    /// A lease, session, worker, or runtime identity mismatched.
    IdentityMismatch,
    /// The operation is invalid in the current runtime phase.
    InvalidState,
    /// The request is malformed or violates protocol rules.
    InvalidRequest,
    /// A data token was unknown, expired, or already redeemed.
    InvalidDataToken,
    /// A requested input deduplication outcome was evicted.
    WriteOutcomeUnknown,
    /// The runtime encountered an internal fault.
    RuntimeFault,
    /// The selected private protocol lacks a requested capability.
    WorkerFeatureUnavailable,
    /// A bounded observation request exceeded worker policy.
    ObservationLimitExceeded,
}

/// Describes output evicted before a requested cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputGap {
    /// First byte no longer retained.
    pub missing_start: u64,
    /// Offset immediately after the missing range.
    pub missing_end: u64,
}

/// Carries a structured worker request failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{code:?}: {message}")]
pub struct ControlError {
    /// Machine-readable failure code.
    pub code: ControlCode,
    /// Sanitized human-readable explanation.
    pub message: String,
    /// Whether repeating the same request may succeed.
    pub retryable: bool,
}

/// Defines one sequenced asynchronous worker event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlEvent {
    /// Worker-lifetime monotonic event sequence.
    pub event_sequence: u64,
    /// Event payload.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Enumerates asynchronous worker state changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// The PTY child started.
    RuntimeStarted {
        /// New runtime generation.
        runtime_id: RuntimeId,
        /// Root child process.
        child_process: ProcessIdentity,
    },
    /// New PTY output advanced the watermark.
    OutputAdvanced {
        /// Runtime generation.
        runtime_id: RuntimeId,
        /// Offset after the newest byte.
        next_offset: u64,
    },
    /// Current visible terminal state changed.
    TerminalChanged {
        /// Runtime generation.
        runtime_id: RuntimeId,
        /// Snapshot watermark.
        watermark: u64,
    },
    /// Worker-local provider identity state changed.
    IdentityChanged {
        /// Runtime generation.
        runtime_id: RuntimeId,
    },
    /// Root child reached a terminal outcome.
    ChildExited {
        /// Runtime generation.
        runtime_id: RuntimeId,
        /// Recorded terminal outcome.
        exit: ExitStatus,
    },
    /// Worker runtime entered a faulted phase.
    RuntimeFault {
        /// Runtime generation when one exists.
        runtime_id: Option<RuntimeId>,
        /// Sanitized typed failure.
        error: ControlError,
    },
}

/// Carries any message valid on the leased control connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlMessage {
    /// Daemon-to-worker request.
    Request(ControlRequest),
    /// Worker-to-daemon response.
    Response(ControlResponse),
    /// Worker-to-daemon asynchronous event.
    Event(ControlEvent),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::CURRENT_VERSION;

    fn initialize(secret: &str) -> Initialize {
        Initialize {
            session_id: SessionId::new("s-1").expect("valid session"),
            transaction_id: TransactionId::new("tx-1").expect("valid transaction"),
            expected_worker_id: WorkerId::new("w-1").expect("valid worker"),
            launch: LaunchIdentity {
                agent: "codex".to_owned(),
                agent_base: "codex".to_owned(),
                reference_kind: Some("id".to_owned()),
            },
            executable: PathBuf::from(format!("/secret/{secret}")),
            arguments: vec![secret.to_owned()],
            cwd: PathBuf::from(format!("/secret/{secret}")),
            dimensions: Dimensions::new(120, 40).expect("valid dimensions"),
            environment: SecretEnv::new(BTreeMap::from([("TOKEN".to_owned(), secret.to_owned())]))
                .expect("valid environment"),
            limits: InitializeLimits::new(1024, 1024, 32, 60_000).expect("valid limits"),
            stop_policy: StopPolicy::new(5_000).expect("valid policy"),
            hook_protocol_version: CURRENT_VERSION,
            public_protocol_version: 1,
        }
    }

    #[test]
    fn initialize_debug_redacts_all_launch_secrets() {
        let secret = "seeded_value_that_must_not_leak";
        let rendered = format!("{:?}", initialize(secret));

        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn input_debug_redacts_fragment_contents() {
        let secret = "input_that_must_not_leak";
        let plan = InputPlan::new(
            WriteId::new("write-1").expect("valid write"),
            vec![InputFragment {
                bytes: SecretBytes::new(secret.as_bytes().to_vec()),
                delay_after_ms: 10,
            }],
        )
        .expect("valid input plan");

        let rendered = format!("{plan:?}");
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn untagged_control_message_round_trips_a_negotiation() {
        let message = ControlMessage::Request(ControlRequest {
            request_id: RequestId::new("request-1").expect("valid request"),
            kind: RequestKind::Negotiate {
                daemon_instance_id: DaemonId::new("daemon-1").expect("valid daemon"),
                minimum_version: crate::PREVIOUS_VERSION,
                maximum_version: crate::CURRENT_VERSION,
            },
        });
        let json = serde_json::to_string(&message).expect("serialize message");
        let decoded: ControlMessage = serde_json::from_str(&json).expect("deserialize message");

        assert_eq!(decoded, message);
        assert!(json.contains(r#""type":"negotiate""#));
    }

    #[test]
    fn attach_start_without_dimensions_is_additive() {
        let start = AttachStart { dimensions: None };

        assert_eq!(
            serde_json::to_value(start).expect("serialize attach start"),
            serde_json::json!({})
        );
    }

    #[test]
    fn invalid_strong_values_fail_control_deserialization() {
        let error = serde_json::from_str::<Dimensions>(r#"{"columns":0,"rows":24}"#)
            .expect_err("zero columns must fail");

        assert!(error.to_string().contains("must be nonzero"));
    }

    #[test]
    fn observation_messages_round_trip_without_embedding_pty_bytes() {
        let scope = RuntimeScope {
            lease_id: LeaseId::new("lease-1").expect("valid lease"),
            session_id: SessionId::new("s-1").expect("valid session"),
            worker_id: WorkerId::new("w-1").expect("valid worker"),
            runtime_id: RuntimeId::new("runtime-1").expect("valid runtime"),
        };
        let request = ControlMessage::Request(ControlRequest {
            request_id: RequestId::new("observation-1").expect("valid request"),
            kind: RequestKind::ReadOutput {
                scope: scope.clone(),
                stream_id: StreamId::new("observation-stream-1").expect("valid stream"),
                after_offset: Some(9),
                max_bytes: 128,
                wait_ms: 50,
            },
        });
        let encoded = serde_json::to_string(&request).expect("serialize request");
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).expect("deserialize request"),
            request
        );

        let response = ResponseKind::OutputReadOpened {
            token: DataToken::new("observation-token").expect("valid token"),
            expires_at_ms: 500,
        };
        let rendered = format!("{response:?}");
        assert!(rendered.contains("OutputReadOpened"));
        assert!(!rendered.contains("pty-data"));
    }
}
