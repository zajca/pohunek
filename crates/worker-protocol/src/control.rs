//! Typed worker control messages.
//!
//! Requests, responses, and asynchronous events share one bounded NDJSON
//! connection. Unknown additive object fields are ignored by serde, while
//! unknown operations fail deserialization and affect only that connection.

// Rust guideline compliant 2026-10-14

use std::fmt::{Debug, Formatter};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    BaseEnv, DaemonId, DataToken, LeaseChallenge, LeaseId, RequestId, SecretBytes, SecretEnv,
    SessionId, StreamId, TransactionId, Version, VersionRange, WorkerId, WorkerInstanceId, WriteId,
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
    /// Durable provider-managed subagent lifecycle observation.
    SubagentObservation,
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
    /// A stop grace period was zero, or the wait for the root's death after
    /// the forced kill was shorter than the grace.
    #[error("stop grace period must be nonzero and no longer than the post-kill reap deadline")]
    InvalidStopPolicy,
    /// An input stall bound was zero.
    #[error("input stall bound must be nonzero")]
    InvalidInputPolicy,
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

/// Latest native reference the designated launch process reported.
///
/// The worker journals it when it accepts a report of the launch provider from
/// the verified launch process, and keeps it independently of the claim's
/// lease, so a conversation switch survives a daemon outage.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedNativeReference {
    /// Stable provider base.
    pub provider: String,
    /// Verified launch process identity that reported it.
    pub process: ProcessIdentity,
    /// Monotonic hook sequence of the report.
    pub sequence: u64,
    /// Native recovery reference kind.
    pub reference_kind: String,
    /// Provider-native recovery reference.
    pub native_reference: String,
}

impl Debug for ReportedNativeReference {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportedNativeReference")
            .field("provider", &self.provider)
            .field("process", &self.process)
            .field("sequence", &self.sequence)
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
    /// Opaque profile label of a forwarded claim, absent for a hook claim.
    ///
    /// The worker stores it verbatim and never interprets it. Withheld from a
    /// connection that negotiated below [`crate::ACTIVE_IDENTITY_FORWARD_VERSION`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// How the worker came to hold the claim. Absent means [`ClaimOrigin::Hook`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ClaimOrigin>,
    /// Public session annotation of a forwarded claim. It is never a verified
    /// native reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_session: Option<ReportedSession>,
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
            .field("agent", &self.agent)
            .field("origin", &self.origin)
            .field("reported_session", &self.reported_session)
            .finish()
    }
}

/// How the worker came to hold an active identity claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOrigin {
    /// A managed hook reported it over the worker's identity socket.
    Hook,
    /// The daemon forwarded a public report with
    /// [`RequestKind::RecordActiveIdentity`].
    Forwarded,
}

/// Selects how a [`ReportedSession`] value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedSessionKind {
    /// An opaque provider session id.
    Id,
    /// A path to a provider session file.
    Path,
}

/// Public session annotation a forwarded report carries.
///
/// The worker journals it beside the claim and hands it back unchanged. It
/// never feeds launch identity, a native reference or a resume binding.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedSession {
    /// Whether `value` is an id or a path.
    pub kind: ReportedSessionKind,
    /// The id or path.
    pub value: String,
}

impl ReportedSession {
    /// Whether `value` fits the bound of its kind and holds no control
    /// character.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let limit = match self.kind {
            ReportedSessionKind::Id => crate::MAX_REPORTED_SESSION_ID_BYTES,
            ReportedSessionKind::Path => crate::MAX_REPORTED_SESSION_PATH_BYTES,
        };
        !self.value.is_empty()
            && self.value.len() <= limit
            && !self.value.chars().any(char::is_control)
    }
}

impl Debug for ReportedSession {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportedSession")
            .field("kind", &self.kind)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// A public `session.report_agent` the daemon forwards into the worker.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardedActiveReport {
    /// Runtime base of the reporting provider.
    pub provider: String,
    /// Opaque profile label, omitted when it equals `provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Subject process of the report.
    pub process: ProcessIdentity,
    /// The hook's `CLOCK_MONOTONIC` value, never minted by the daemon.
    pub sequence: u64,
    /// RFC 3339 expiry: daemon receipt time plus the shared claim ceiling.
    pub expires_at: String,
    /// Public session annotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_session: Option<ReportedSession>,
}

impl Debug for ForwardedActiveReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardedActiveReport")
            .field("provider", &self.provider)
            .field("agent", &self.agent)
            .field("process", &self.process)
            .field("sequence", &self.sequence)
            .field("expires_at", &self.expires_at)
            .field("reported_session", &self.reported_session)
            .finish()
    }
}

/// A public `session.release_agent` the daemon forwards into the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardedActiveRelease {
    /// Runtime base of the releasing provider.
    pub provider: String,
    /// Subject process of the release.
    pub process: ProcessIdentity,
    /// The hook's `CLOCK_MONOTONIC` value.
    pub sequence: u64,
}

/// Why the worker refused a forwarded report or release.
///
/// Mirrors the subject-side codes of the worker's own identity rejections; the
/// peer-side codes cannot occur because the peer is the lease-authenticated
/// daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityRejectReason {
    /// The scope names another runtime generation.
    RuntimeMismatch,
    /// The runtime is not running.
    PhaseNotRunning,
    /// The session's hook schema does not admit the provider.
    ProviderNotAllowed,
    /// The session's hook schema does not admit the operation.
    ActionNotAllowed,
    /// The claim is already expired or exceeds the shared ceiling.
    ClaimExpired,
    /// The subject is gone, replaced or outside the managed PTY tree.
    ReportedProcessInvalid,
    /// The sequence is not ahead of the recorded ordering of the subject.
    SequenceStale,
    /// The subject has no ordering mark and the marks fill the worker's bound.
    IdentityReportersExhausted,
    /// The release names no active claim of that provider and process.
    ReleaseUnmatched,
    /// The label or the session annotation is outside its bound.
    ForwardedFieldInvalid,
}

/// Result of a forwarded report or release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActiveIdentityOutcome {
    /// The worker journaled the operation and acknowledged it afterwards.
    Accepted {
        /// Revision of the worker's active identity after the operation.
        revision: u64,
    },
    /// The worker refused the operation and changed nothing.
    Rejected {
        /// Why.
        reason: IdentityRejectReason,
    },
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
    /// How long after the forced kill the root's death is awaited. A daemon of
    /// the previous release does not send it; the worker then uses its default.
    #[serde(skip_serializing_if = "Option::is_none")]
    kill_reap_ms: Option<u64>,
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
            Ok(Self {
                grace_ms,
                kill_reap_ms: None,
            })
        }
    }

    /// Creates a stop policy that also bounds the wait for the root's death
    /// after the forced kill.
    ///
    /// # Errors
    ///
    /// Returns [`ControlTypeError::InvalidStopPolicy`] when `grace_ms` is zero
    /// or `kill_reap_ms` is shorter than `grace_ms`.
    pub const fn with_kill_reap(
        grace_ms: u64,
        kill_reap_ms: u64,
    ) -> Result<Self, ControlTypeError> {
        if grace_ms == 0 || kill_reap_ms < grace_ms {
            Err(ControlTypeError::InvalidStopPolicy)
        } else {
            Ok(Self {
                grace_ms,
                kill_reap_ms: Some(kill_reap_ms),
            })
        }
    }

    /// Returns the termination grace period in milliseconds.
    #[must_use]
    pub const fn grace_ms(self) -> u64 {
        self.grace_ms
    }

    /// Returns how long after the forced kill the root's death is awaited, in
    /// milliseconds, when the policy carries it.
    #[must_use]
    pub const fn kill_reap_ms(self) -> Option<u64> {
        self.kill_reap_ms
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
            #[serde(default)]
            kill_reap_ms: Option<u64>,
        }

        let policy = WireStopPolicy::deserialize(deserializer)?;
        match policy.kill_reap_ms {
            Some(kill_reap_ms) => Self::with_kill_reap(policy.grace_ms, kill_reap_ms),
            None => Self::new(policy.grace_ms),
        }
        .map_err(serde::de::Error::custom)
    }
}

/// Bounds how long PTY input may wait for the child to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct InputPolicy {
    /// Longest time one input write may go without the PTY accepting a byte.
    stall_ms: u64,
}

impl InputPolicy {
    /// Creates a nonzero input stall bound.
    ///
    /// # Errors
    ///
    /// Returns [`ControlTypeError::InvalidInputPolicy`] when `stall_ms` is zero.
    pub const fn new(stall_ms: u64) -> Result<Self, ControlTypeError> {
        if stall_ms == 0 {
            Err(ControlTypeError::InvalidInputPolicy)
        } else {
            Ok(Self { stall_ms })
        }
    }

    /// Returns the input stall bound in milliseconds.
    #[must_use]
    pub const fn stall_ms(self) -> u64 {
        self.stall_ms
    }
}

impl<'de> Deserialize<'de> for InputPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireInputPolicy {
            stall_ms: u64,
        }

        let policy = WireInputPolicy::deserialize(deserializer)?;
        Self::new(policy.stall_ms).map_err(serde::de::Error::custom)
    }
}

/// Supplies the one-shot worker launch plan.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Initialize {
    /// Durable logical session.
    pub session_id: SessionId,
    /// Daemon instance that launched this child. Older peers omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_instance_id: Option<DaemonId>,
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
    /// Allowlisted non-secret environment placed under the child.
    pub base_environment: BaseEnv,
    /// Bounded worker memory and retention configuration.
    pub limits: InitializeLimits,
    /// Explicit process-group stop policy.
    pub stop_policy: StopPolicy,
    /// PTY input stall bound.
    pub input_policy: InputPolicy,
    /// Worker-local identity-hook protocol version.
    pub hook_protocol_version: Version,
    /// Public daemon RPC protocol version the managed child's provider hooks
    /// must advertise (`POHUNEK_PROTOCOL_VERSION`). Distinct from
    /// `hook_protocol_version`, which negotiates this private worker
    /// protocol; this crate does not depend on the public `protocol` crate,
    /// so the daemon threads the value through as a plain integer.
    pub public_protocol_version: u32,
    /// Id of the hook schema resolved from the session's pinned runtime.
    ///
    /// Absent for a runtime without an integration, in which case the worker
    /// admits no hook report. A peer that predates the field ignores it and
    /// keeps the validation it was built with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_schema: Option<String>,
}

impl Debug for Initialize {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Initialize")
            .field("session_id", &self.session_id)
            .field("daemon_instance_id", &self.daemon_instance_id)
            .field("transaction_id", &self.transaction_id)
            .field("expected_worker_id", &self.expected_worker_id)
            .field("launch", &self.launch)
            .field("executable", &"<redacted>")
            .field("argument_count", &self.arguments.len())
            .field("cwd", &"<redacted>")
            .field("dimensions", &self.dimensions)
            .field("environment", &self.environment)
            .field("base_environment", &self.base_environment)
            .field("limits", &self.limits)
            .field("stop_policy", &self.stop_policy)
            .field("input_policy", &self.input_policy)
            .field("hook_protocol_version", &self.hook_protocol_version)
            .field("public_protocol_version", &self.public_protocol_version)
            .field("hook_schema", &self.hook_schema)
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
    #[serde(rename = "runtime_id")]
    pub worker_instance_id: WorkerInstanceId,
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
    /// Whether the PTY output was force-closed before it reached EOF.
    ///
    /// A process outside the root's process group held the PTY open past the
    /// stop deadline, so output written after the close is not retained. The
    /// exit itself is exact. A status without the field, as a previous
    /// release's worker sends it, reads as `false`.
    #[serde(default)]
    pub output_forced_closed: bool,
}

/// Lifecycle of one provider-managed subagent in the worker snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentPhase {
    /// The subagent is running.
    Running,
    /// The subagent completed successfully.
    Completed,
    /// The subagent failed.
    Failed,
    /// The subagent was cancelled.
    Cancelled,
    /// The owning runtime ended before completion was reported.
    Lost,
}

/// Sanitized durable state for one provider-managed subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    /// Provider-native lifecycle correlation identifier.
    pub id: String,
    /// Parent subagent identifier, when reported.
    pub parent_id: Option<String>,
    /// Provider base name.
    pub provider: String,
    /// Provider-defined subagent type, when reported.
    pub agent_type: Option<String>,
    /// Current lifecycle phase.
    pub phase: SubagentPhase,
    /// Worker-owned monotonic revision.
    pub revision: u64,
    /// First accepted start timestamp.
    pub started_at_ms: u64,
    /// Latest accepted transition timestamp.
    pub updated_at_ms: u64,
    /// Terminal transition timestamp, when terminal.
    pub finished_at_ms: Option<u64>,
}

/// Most background items one snapshot may carry.
///
/// Claude Code lists its live background tasks at each turn end, and a
/// session rarely runs more than a few dozen at once. 64 keeps the snapshot
/// small beside the subagent rows, which are bounded by the worker's active
/// and terminal capacities. A longer snapshot is not applied, so the previous
/// one stays, marked by its observation time.
pub const BACKGROUND_MAX_ITEMS: usize = 64;
/// Byte bound of a background item label, which is the sanitized description.
///
/// A description is a short phrase; 160 bytes keeps it readable in a list
/// line and bounds the snapshot's size with the other fields.
pub const BACKGROUND_LABEL_MAX_BYTES: usize = 160;
/// Byte bound of a background item's program name.
///
/// The program is the basename of a shell's first command word. Names such as
/// `cargo`, `bacon` or `npm` are short; 64 bytes admits longer tool names while
/// keeping arguments and paths out of the field.
pub const BACKGROUND_PROGRAM_MAX_BYTES: usize = 64;
/// Byte bound of a provider task id, which is also the bound of a subagent id.
///
/// A background item id is the provider task id, and for a background subagent
/// it equals the subagent id. Provider ids are UUID-like strings; 256 bytes
/// leaves room for longer ids. A snapshot task with a longer id is refused
/// whole by the worker, so no stored item exceeds this bound. The daemon
/// re-checks stored items against this constant.
pub const BACKGROUND_TASK_ID_MAX_BYTES: usize = 256;
/// Byte bound of a provider task type label, as in `subagent`, `shell` or `MCP task`.
///
/// Claude Code's type labels are short words; 64 bytes leaves room for labels
/// added later. The worker keeps a type label as the item's `provider_kind`
/// only when it is within this bound, and refuses a longer one whole.
pub const BACKGROUND_TASK_TYPE_MAX_BYTES: usize = 64;
/// Byte bound of a provider task status, as in `running` or `pending`.
///
/// Claude Code's statuses are short words; 32 bytes leaves room for statuses
/// added later. A longer status is refused whole by the worker.
pub const BACKGROUND_TASK_STATUS_MAX_BYTES: usize = 32;
/// Byte bound of a provider agent type of a background subagent.
///
/// Agent types are short names such as `general-purpose`; 128 bytes leaves
/// room for longer names. The same bound applies to every subagent claim, so
/// the worker drops a longer agent type from the background item alone.
pub const BACKGROUND_AGENT_TYPE_MAX_BYTES: usize = 128;

/// Latest background-work snapshot of the session's main agent.
///
/// Replaced as a whole by each applied snapshot. Every item reports what the
/// provider listed; the worker never stores arguments or full commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundSnapshot {
    /// Worker-owned revision, bumped by every applied snapshot and by each
    /// clear, so a client detects any change by comparing it.
    pub revision: u64,
    /// Millisecond Unix timestamp of the snapshot the worker applied.
    pub observed_at_ms: u64,
    /// Background items the snapshot listed, at most [`BACKGROUND_MAX_ITEMS`].
    pub items: Vec<BackgroundItem>,
}

/// One background task the provider listed, as the worker keeps it.
///
/// The label and program are shown as they were kept, so `Debug` redacts
/// them and the `Debug` output of a journal or inspect snapshot stays free of
/// task text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundItem {
    /// Provider task id; for a background subagent it equals the subagent id.
    pub id: String,
    /// Closed kind of the task.
    pub kind: BackgroundKind,
    /// Provider's own type label, present only when `kind` is
    /// [`BackgroundKind::Other`].
    pub provider_kind: Option<String>,
    /// Closed status of the task.
    pub status: BackgroundStatus,
    /// Provider agent type, present only for a [`BackgroundKind::Subagent`].
    pub agent_type: Option<String>,
    /// Sanitized, bounded task description.
    pub label: Option<String>,
    /// Program name of a [`BackgroundKind::Shell`]'s command, never its
    /// arguments.
    pub program: Option<String>,
    /// Millisecond Unix timestamp of the first snapshot that listed the task.
    /// The provider reports no start time, so this is the earliest the worker
    /// knows of.
    pub first_seen_ms: u64,
    /// Millisecond Unix timestamp of the latest snapshot that listed the task.
    pub last_seen_ms: u64,
}

impl Debug for BackgroundItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundItem")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("provider_kind", &self.provider_kind)
            .field("status", &self.status)
            .field("agent_type", &self.agent_type)
            .field("label", &self.label.as_ref().map(|_| "[REDACTED]"))
            .field("program", &self.program.as_ref().map(|_| "[REDACTED]"))
            .field("first_seen_ms", &self.first_seen_ms)
            .field("last_seen_ms", &self.last_seen_ms)
            .finish()
    }
}

/// Closed kind of a background task.
///
/// Each variant is one of the task types Claude Code reports. A type this
/// build does not know is [`BackgroundKind::Other`], which keeps the
/// provider's label in [`BackgroundItem::provider_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    /// A background agent.
    Subagent,
    /// A workflow run.
    Workflow,
    /// A background shell command.
    Shell,
    /// A monitor watching an MCP or WebSocket source.
    Monitor,
    /// A background MCP tool call.
    McpTask,
    /// A teammate agent.
    Teammate,
    /// A background memory consolidation.
    Dream,
    /// An auto-mode scan.
    AutoModeScan,
    /// A memory import.
    MemoryImport,
    /// A cloud session.
    CloudSession,
    /// A type this build does not know.
    Other,
}

/// Closed status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundStatus {
    /// The task is running.
    Running,
    /// The task is queued and not yet running.
    Pending,
    /// Any other status the provider reports.
    Other,
}

/// Reports current worker and runtime authority facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectSnapshot {
    /// Durable logical session.
    pub session_id: SessionId,
    /// Worker process identity.
    pub worker_id: WorkerId,
    /// Runtime generation when initialized.
    #[serde(rename = "runtime_id")]
    pub worker_instance_id: Option<WorkerInstanceId>,
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
    /// Latest native reference the verified launch process reported, kept
    /// beyond the active claim's lease and release. Absent for a worker that
    /// predates it, whose snapshot the daemon then reads through the launch
    /// and active claims alone.
    #[serde(default)]
    pub native_reference: Option<ReportedNativeReference>,
    /// Durable provider-managed subagent state.
    #[serde(default)]
    pub subagents: Vec<SubagentSnapshot>,
    /// Hook schema id the worker validates reports with, journaled at
    /// initialization. Absent for a worker that predates schema delivery or
    /// whose runtime has no integration.
    #[serde(default)]
    pub hook_schema: Option<String>,
    /// Latest background-work snapshot of the session's main agent, absent
    /// when none was applied or the last one was cleared.
    #[serde(default)]
    pub background: Option<BackgroundSnapshot>,
    /// Number of journaled changes to the active identity or its release,
    /// counting hook and forwarded operations alike. Absent on a connection
    /// that negotiated below [`crate::ACTIVE_IDENTITY_FORWARD_VERSION`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_identity_revision: Option<u64>,
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
        /// Sensitive launch plan, boxed because it dwarfs every other request.
        initialize: Box<Initialize>,
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
    /// Records a public agent report as the session's active identity.
    ///
    /// Sent only on connections that negotiated at least
    /// [`crate::ACTIVE_IDENTITY_FORWARD_VERSION`].
    RecordActiveIdentity {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// The report to validate and journal.
        report: ForwardedActiveReport,
    },
    /// Releases the session's active identity on a public release.
    ///
    /// Sent only on connections that negotiated at least
    /// [`crate::ACTIVE_IDENTITY_FORWARD_VERSION`].
    ReleaseActiveIdentity {
        /// Exact leased runtime scope.
        scope: RuntimeScope,
        /// The release to validate and journal.
        release: ForwardedActiveRelease,
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
        #[serde(rename = "runtime_id")]
        worker_instance_id: Option<WorkerInstanceId>,
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
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
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
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
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
    /// Answers a forwarded report or release.
    ActiveIdentityRecorded {
        /// Whether the worker journaled the operation.
        outcome: ActiveIdentityOutcome,
    },
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
    /// PTY input made no progress within the input stall bound.
    ///
    /// Part of the plan may already have reached the child.
    InputStalled,
    /// The launch working directory is not an existing directory, so no child
    /// process was spawned.
    LaunchCwdMissing,
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
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
        /// Root child process.
        child_process: ProcessIdentity,
    },
    /// New PTY output advanced the watermark.
    OutputAdvanced {
        /// Runtime generation.
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
        /// Offset after the newest byte.
        next_offset: u64,
    },
    /// Current visible terminal state changed.
    TerminalChanged {
        /// Runtime generation.
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
        /// Snapshot watermark.
        watermark: u64,
    },
    /// Worker-local provider identity state changed.
    IdentityChanged {
        /// Runtime generation.
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
    },
    /// Provider-managed subagent state changed.
    SubagentsChanged {
        /// Runtime generation.
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
    },
    /// Root child reached a terminal outcome.
    ChildExited {
        /// Runtime generation.
        #[serde(rename = "runtime_id")]
        worker_instance_id: WorkerInstanceId,
        /// Recorded terminal outcome.
        exit: ExitStatus,
    },
    /// Worker runtime entered a faulted phase.
    RuntimeFault {
        /// Runtime generation when one exists.
        #[serde(rename = "runtime_id")]
        worker_instance_id: Option<WorkerInstanceId>,
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
            daemon_instance_id: Some(DaemonId::new("daemon-1").expect("valid daemon")),
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
            base_environment: BaseEnv::new(BTreeMap::from([(
                "HOME".to_owned(),
                format!("/home/{secret}"),
            )]))
            .expect("valid base environment"),
            limits: InitializeLimits::new(1024, 1024, 32, 60_000).expect("valid limits"),
            stop_policy: StopPolicy::new(5_000).expect("valid policy"),
            input_policy: InputPolicy::new(3_000).expect("valid policy"),
            hook_protocol_version: CURRENT_VERSION,
            public_protocol_version: 1,
            hook_schema: None,
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
    fn initialize_hook_schema_is_additive_on_the_wire() {
        let without = initialize("value");
        let wire = serde_json::to_value(&without).expect("serialize");
        assert!(wire.get("hook_schema").is_none());
        let parsed: Initialize = serde_json::from_value(wire).expect("peer without the field");
        assert_eq!(parsed.hook_schema, None);

        let with = Initialize {
            hook_schema: Some("identity-v1".to_owned()),
            ..initialize("value")
        };
        let wire = serde_json::to_value(&with).expect("serialize");
        assert_eq!(wire["hook_schema"], "identity-v1");
        let parsed: Initialize = serde_json::from_value(wire).expect("round trip");
        assert_eq!(parsed.hook_schema.as_deref(), Some("identity-v1"));
    }

    #[test]
    fn initialize_daemon_identity_accepts_older_peer_and_rejects_invalid_identity() {
        let mut wire = serde_json::to_value(initialize("value")).expect("serialize");
        wire.as_object_mut()
            .expect("initialize object")
            .remove("daemon_instance_id");
        let older: Initialize = serde_json::from_value(wire).expect("older daemon initialize");
        assert_eq!(older.daemon_instance_id, None);

        let mut wire = serde_json::to_value(initialize("value")).expect("serialize");
        wire["daemon_instance_id"] = serde_json::json!("invalid/daemon");
        assert!(
            serde_json::from_value::<Initialize>(wire).is_err(),
            "the launch daemon identity must be validated"
        );
    }

    #[test]
    fn initialize_wire_requires_the_base_environment_and_the_input_policy() {
        let wire = serde_json::to_value(initialize("value")).expect("serialize initialize");
        assert_eq!(wire["base_environment"]["HOME"], "/home/value");
        assert_eq!(wire["input_policy"]["stall_ms"], 3_000);
        assert_eq!(
            serde_json::from_value::<Initialize>(wire.clone()).expect("round trip"),
            initialize("value")
        );

        for field in ["base_environment", "input_policy"] {
            let mut incomplete = wire.clone();
            incomplete
                .as_object_mut()
                .expect("initialize object")
                .remove(field);
            assert!(
                serde_json::from_value::<Initialize>(incomplete).is_err(),
                "an initialize without {field} must be refused"
            );
        }
    }

    #[test]
    fn initialize_deserialization_revalidates_the_base_environment() {
        let mut json = serde_json::to_value(initialize("value")).expect("serialize initialize");
        json["base_environment"] = serde_json::json!({ "NOTIFY_SOCKET": "/run/notify" });

        serde_json::from_value::<Initialize>(json).expect_err("denylisted base must fail");
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
}
