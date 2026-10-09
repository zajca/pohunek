//! Strong identifiers used by the worker protocol.
//!
//! Identifiers are validated before deserialization succeeds. Their restricted
//! ASCII representation is safe to compare and suitable for use as a component
//! after the paths crate performs its own path-specific validation.

// Rust guideline compliant 2026-06-26

use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Maximum bytes accepted in a worker-protocol identifier.
///
/// The limit keeps attacker-controlled control messages bounded while leaving
/// ample room for generated UUIDs and prefixed logical identifiers.
const MAX_ID_BYTES: usize = 128;

/// Reports an invalid worker-protocol identifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// The identifier was empty.
    #[error("worker-protocol identifier must not be empty")]
    Empty,
    /// The identifier exceeded the protocol limit.
    #[error("worker-protocol identifier is {actual} bytes; maximum is {maximum}")]
    TooLong {
        /// Observed byte length.
        actual: usize,
        /// Maximum accepted byte length.
        maximum: usize,
    },
    /// The identifier contained a disallowed byte.
    #[error("worker-protocol identifier contains a disallowed byte at index {index}")]
    InvalidByte {
        /// Byte index of the invalid value.
        index: usize,
    },
    /// The identifier was a reserved path component.
    #[error("worker-protocol identifier is a reserved path component")]
    Reserved,
}

fn validate(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > MAX_ID_BYTES {
        return Err(IdError::TooLong {
            actual: value.len(),
            maximum: MAX_ID_BYTES,
        });
    }
    if matches!(value, "." | "..") {
        return Err(IdError::Reserved);
    }

    if let Some((index, _)) = value
        .bytes()
        .enumerate()
        .find(|(_, byte)| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(IdError::InvalidByte { index });
    }

    Ok(())
}

macro_rules! define_id {
    ($name:ident, $summary:literal) => {
        #[doc = $summary]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a validated identifier.
            ///
            /// # Errors
            ///
            /// Returns [`IdError`] when `value` is empty, too long, or contains
            /// bytes outside the worker-protocol identifier alphabet.
            pub fn new(value: impl AsRef<str>) -> Result<Self, IdError> {
                let value = value.as_ref();
                validate(value)?;
                Ok(Self(value.to_owned()))
            }

            /// Returns the validated string representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                validate(&value)?;
                Ok(Self(value))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::try_from(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(SessionId, "Identifies one durable logical session.");
define_id!(WorkerId, "Identifies one session-worker process.");
define_id!(
    WorkerInstanceId,
    "Identifies one worker instance, one uninterrupted PTY generation.\n\n\
     Every serialized field of this type uses the key `runtime_id`; the key is\n\
     fixed so a live durable worker and a newer daemon read each other's messages."
);
define_id!(DaemonId, "Identifies one daemon process instance.");
define_id!(RequestId, "Correlates one control request and response.");
define_id!(LeaseId, "Identifies the current controller lease.");
define_id!(
    TransactionId,
    "Identifies one idempotent logical transaction."
);
define_id!(StreamId, "Identifies one framed data stream.");
define_id!(WriteId, "Identifies one deduplicated PTY input plan.");
