//! Protocol version constant and negotiation.
//!
//! On connect, client and daemon exchange inclusive supported-version ranges
//! and select their highest common version. A genuinely incompatible pair fails with a typed
//! `daemon/version_mismatch` error rather than undefined behavior (see
//! `docs/plan-phase-1.md` "Control Protocol" and `docs/architecture.md`
//! "Protocol versioning").

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::error::ProtocolError;

/// The protocol version this build speaks.
///
/// Bump this when the wire contract changes in a way that is not purely
/// additive. New optional fields and methods do not require a bump when their
/// containing contract explicitly permits them. A bump deletes the adapter for
/// the version that falls out of the window and adds one for the version it
/// leaves behind (see [`crate::compat`]).
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(4);

/// Oldest public protocol version a daemon of this build accepts.
///
/// One below [`PROTOCOL_VERSION`]: the daemon serves the previous version
/// through the shape-only adapters in [`crate::compat`]. A semantic change
/// (new required parameter, removed method, changed error code or meaning)
/// cannot be adapted and raises this minimum to [`PROTOCOL_VERSION`].
pub const MIN_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(PROTOCOL_VERSION.0 - 1);

/// Inclusive public protocol range of this build: the window.
///
/// The one definition of the window. A daemon negotiates it with every client,
/// and every client advertises it ([`CLIENT_PROTOCOL_VERSIONS`]). Both sides
/// translate the previous version through the shape-only adapters in
/// [`crate::compat`]: the daemon converts a previous-version peer's requests and
/// results, a client converts them for a previous-version daemon.
pub const SUPPORTED_PROTOCOL_VERSIONS: ProtocolVersionRange = ProtocolVersionRange {
    minimum: MIN_PROTOCOL_VERSION,
    maximum: PROTOCOL_VERSION,
};

/// Inclusive public protocol range a client of this build advertises.
///
/// Clients (CLI, Rust client) carry the same adapters as the daemon, so they
/// offer the whole window. A daemon of the previous release answers in the
/// previous version and the client translates it to the current shape; a
/// method the previous version never defined fails on the client with
/// `daemon/daemon_protocol_too_old` before anything is sent. The TypeScript SDK
/// carries no adapter and offers [`CURRENT_PROTOCOL_VERSIONS`] only.
pub const CLIENT_PROTOCOL_VERSIONS: ProtocolVersionRange = SUPPORTED_PROTOCOL_VERSIONS;

/// The exact current version as a range.
///
/// Describes the shape a client hands to its callers: client adapters
/// translate every older daemon payload to the current shape. New Hermes
/// policies name exactly this version; the CLI JSON envelope advertises its
/// full supported window while retaining the previous identity spelling at
/// typed session sites for an installed previous-release plugin.
pub const CURRENT_PROTOCOL_VERSIONS: ProtocolVersionRange = ProtocolVersionRange {
    minimum: PROTOCOL_VERSION,
    maximum: PROTOCOL_VERSION,
};

// The window needs a nonzero previous version to adapt.
const _: () = assert!(
    PROTOCOL_VERSION.0 >= 2,
    "the window needs a previous version"
);

/// A protocol version number selected for responses and events.
///
/// Wrapped in a newtype so it serializes transparently as an integer on the
/// wire (`"v": 2`) while remaining type-safe in Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProtocolVersion.ts"))]
pub struct ProtocolVersion(u32);

impl ProtocolVersion {
    /// Creates a nonzero public protocol version.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolVersionError::Zero`] when `value` is zero.
    pub const fn new(value: u32) -> Result<Self, ProtocolVersionError> {
        if value == 0 {
            Err(ProtocolVersionError::Zero)
        } else {
            Ok(Self(value))
        }
    }
    /// The raw integer value carried on the wire.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for ProtocolVersion {
    type Error = ProtocolVersionError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An inclusive public protocol version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProtocolVersionRange.ts"))]
pub struct ProtocolVersionRange {
    /// Oldest supported version.
    minimum: ProtocolVersion,
    /// Newest supported version.
    maximum: ProtocolVersion,
}

impl ProtocolVersionRange {
    /// Creates an ordered inclusive protocol range.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolVersionError::InvalidRange`] when `minimum` exceeds
    /// `maximum`.
    pub const fn new(
        minimum: ProtocolVersion,
        maximum: ProtocolVersion,
    ) -> Result<Self, ProtocolVersionError> {
        if minimum.0 > maximum.0 {
            Err(ProtocolVersionError::InvalidRange { minimum, maximum })
        } else {
            Ok(Self { minimum, maximum })
        }
    }

    /// Reports whether the range contains `version`.
    #[must_use]
    pub const fn contains(self, version: ProtocolVersion) -> bool {
        self.minimum.0 <= version.0 && version.0 <= self.maximum.0
    }

    /// Returns the oldest supported version.
    #[must_use]
    pub const fn minimum(self) -> ProtocolVersion {
        self.minimum
    }

    /// Returns the newest supported version.
    #[must_use]
    pub const fn maximum(self) -> ProtocolVersion {
        self.maximum
    }
}

impl<'de> Deserialize<'de> for ProtocolVersionRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRange {
            minimum: ProtocolVersion,
            maximum: ProtocolVersion,
        }

        let range = WireRange::deserialize(deserializer)?;
        Self::new(range.minimum, range.maximum).map_err(serde::de::Error::custom)
    }
}

/// Reports invalid or incompatible public protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProtocolVersionError {
    /// Version zero is reserved and invalid.
    #[error("public protocol version must be nonzero")]
    Zero,
    /// A range had its endpoints reversed.
    #[error("public protocol range {minimum}..={maximum} is reversed")]
    InvalidRange {
        /// Requested lower endpoint.
        minimum: ProtocolVersion,
        /// Requested upper endpoint.
        maximum: ProtocolVersion,
    },
}

/// Negotiate the highest protocol version shared by client and daemon.
///
/// # Errors
///
/// Returns [`ProtocolError::version_mismatch`] when the ranges do not overlap.
pub fn negotiate(
    client: ProtocolVersionRange,
    daemon: ProtocolVersionRange,
) -> Result<ProtocolVersion, ProtocolError> {
    let minimum = client.minimum.max(daemon.minimum);
    let maximum = client.maximum.min(daemon.maximum);
    if minimum > maximum {
        Err(ProtocolError::version_mismatch(client, daemon))
    } else {
        Ok(maximum)
    }
}
