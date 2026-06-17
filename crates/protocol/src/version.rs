//! Protocol version constant and negotiation.
//!
//! On connect, client and daemon exchange their protocol versions. New fields
//! are additive and unknown fields are ignored, so peers interoperate on the
//! common subset. A genuinely incompatible pair fails with a typed
//! `daemon/version_mismatch` error rather than undefined behavior (see
//! `docs/plan-phase-1.md` "Control Protocol" and `docs/architecture.md`
//! "Protocol versioning").

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// The protocol version this build speaks.
///
/// Bump this when the wire contract changes in a way that is not purely
/// additive. Additive changes (new optional fields, new methods) do NOT require
/// a bump because unknown fields are ignored by design.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

/// A protocol version number as carried by the `v` field of every envelope.
///
/// Wrapped in a newtype so it serializes transparently as an integer on the
/// wire (`"v": 1`) while remaining type-safe in Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl ProtocolVersion {
    /// The raw integer value carried on the wire.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Negotiate an agreed protocol version between a client and a daemon.
///
/// Compatibility rule for Phase 1: the two versions must share the same major
/// contract, which we model with exact equality of the single integer version.
/// Because changes are additive within a version, equal versions are guaranteed
/// to interoperate. Differing versions are treated as incompatible and produce
/// a typed `version_mismatch` error carrying both versions so the operator (or
/// an operator agent) can see exactly what to upgrade.
///
/// This is intentionally strict now and can be relaxed later (e.g. to a
/// "min supported version" range) without changing call sites, since the return
/// type already distinguishes the agreed version from a typed error.
///
/// # Errors
///
/// Returns [`ProtocolError::version_mismatch`] when `client_v != daemon_v`.
pub fn negotiate(
    client_v: ProtocolVersion,
    daemon_v: ProtocolVersion,
) -> Result<ProtocolVersion, ProtocolError> {
    if client_v == daemon_v {
        Ok(client_v)
    } else {
        Err(ProtocolError::version_mismatch(client_v, daemon_v))
    }
}
