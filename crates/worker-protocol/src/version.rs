//! Worker protocol version negotiation.
//!
//! A daemon and worker select the highest version in their shared inclusive
//! ranges. Every worker-aware release supports the current protocol and the
//! immediately preceding protocol.

// Rust guideline compliant 2026-08-04

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Current worker protocol version.
pub const CURRENT_VERSION: Version = Version(4);

/// Immediately preceding worker protocol version.
pub const PREVIOUS_VERSION: Version = Version(3);

/// First version with atomic attach snapshots.
///
/// Version three adds an attach-start grant that combines the initial terminal
/// dimensions with a forced terminal repaint. Older workers can replay output,
/// but cannot safely reconstruct a TUI across historical resizes.
pub const ATTACH_SNAPSHOT_VERSION: Version = Version(3);

/// First version with bounded control-plane terminal observation.
pub const CONTROL_PLANE_OBSERVATION_VERSION: Version = Version(4);

/// Versions supported by this crate release.
pub const SUPPORTED_RANGE: VersionRange = VersionRange {
    minimum: PREVIOUS_VERSION,
    maximum: CURRENT_VERSION,
};

/// Identifies one worker protocol wire version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Version(u32);

impl Version {
    /// Creates a nonzero protocol version.
    ///
    /// # Errors
    ///
    /// Returns [`VersionError::Zero`] when `value` is zero.
    pub const fn new(value: u32) -> Result<Self, VersionError> {
        if value == 0 {
            Err(VersionError::Zero)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the numeric wire representation.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl TryFrom<u32> for Version {
    type Error = VersionError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Describes an inclusive worker protocol version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VersionRange {
    /// Oldest supported version.
    minimum: Version,
    /// Newest supported version.
    maximum: Version,
}

impl VersionRange {
    /// Creates an ordered inclusive range.
    ///
    /// # Errors
    ///
    /// Returns [`VersionError::InvalidRange`] when `minimum` exceeds `maximum`.
    pub const fn new(minimum: Version, maximum: Version) -> Result<Self, VersionError> {
        if minimum.0 > maximum.0 {
            Err(VersionError::InvalidRange { minimum, maximum })
        } else {
            Ok(Self { minimum, maximum })
        }
    }

    /// Reports whether the range contains `version`.
    #[must_use]
    pub const fn contains(self, version: Version) -> bool {
        self.minimum.0 <= version.0 && version.0 <= self.maximum.0
    }

    /// Returns the oldest supported version.
    #[must_use]
    pub const fn minimum(self) -> Version {
        self.minimum
    }

    /// Returns the newest supported version.
    #[must_use]
    pub const fn maximum(self) -> Version {
        self.maximum
    }
}

impl<'de> Deserialize<'de> for VersionRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRange {
            minimum: Version,
            maximum: Version,
        }

        let range = WireRange::deserialize(deserializer)?;
        Self::new(range.minimum, range.maximum).map_err(serde::de::Error::custom)
    }
}

/// Reports invalid or incompatible protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VersionError {
    /// Version zero is reserved and invalid.
    #[error("worker protocol version must be nonzero")]
    Zero,
    /// A range had its endpoints reversed.
    #[error("worker protocol range {minimum}..={maximum} is reversed")]
    InvalidRange {
        /// Requested lower endpoint.
        minimum: Version,
        /// Requested upper endpoint.
        maximum: Version,
    },
    /// Two valid ranges had no common version.
    #[error(
        "worker protocol ranges {local_minimum}..={local_maximum} and \
         {remote_minimum}..={remote_maximum} do not overlap"
    )]
    Incompatible {
        /// Local lower endpoint.
        local_minimum: Version,
        /// Local upper endpoint.
        local_maximum: Version,
        /// Remote lower endpoint.
        remote_minimum: Version,
        /// Remote upper endpoint.
        remote_maximum: Version,
    },
}

/// Selects the highest version shared by both peers.
///
/// # Errors
///
/// Returns [`VersionError::Incompatible`] when the ranges do not overlap.
pub const fn negotiate(local: VersionRange, remote: VersionRange) -> Result<Version, VersionError> {
    let minimum = if local.minimum.0 > remote.minimum.0 {
        local.minimum
    } else {
        remote.minimum
    };
    let maximum = if local.maximum.0 < remote.maximum.0 {
        local.maximum
    } else {
        remote.maximum
    };

    if minimum.0 > maximum.0 {
        Err(VersionError::Incompatible {
            local_minimum: local.minimum,
            local_maximum: local.maximum,
            remote_minimum: remote.minimum,
            remote_maximum: remote.maximum,
        })
    } else {
        Ok(maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_selects_highest_common_version() {
        let remote = VersionRange::new(PREVIOUS_VERSION, PREVIOUS_VERSION).expect("ordered range");

        assert_eq!(
            negotiate(SUPPORTED_RANGE, remote).expect("compatible ranges"),
            PREVIOUS_VERSION
        );
    }

    #[test]
    fn negotiation_rejects_disjoint_ranges() {
        let remote = VersionRange::new(
            Version::new(5).expect("valid version"),
            Version::new(6).expect("valid version"),
        )
        .expect("ordered range");

        assert!(matches!(
            negotiate(SUPPORTED_RANGE, remote),
            Err(VersionError::Incompatible { .. })
        ));
    }

    #[test]
    fn range_deserialization_rejects_reversed_endpoints() {
        let error = serde_json::from_str::<VersionRange>(r#"{"minimum":2,"maximum":1}"#)
            .expect_err("reversed range must fail");

        assert!(error.to_string().contains("reversed"));
    }
}
