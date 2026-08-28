//! JavaScript-safe decimal wire integers.
//!
//! Long-lived counters may exceed JavaScript's exact integer range. Each type
//! therefore serializes as a canonical unsigned decimal string while exposing
//! checked Rust construction and `u64` access.
//!
//! # Examples
//!
//! ```
//! use protocol::OutputOffset;
//!
//! let offset = OutputOffset::parse("9007199254740993")?;
//! assert_eq!(offset.get(), 9_007_199_254_740_993);
//! assert_eq!(serde_json::to_string(&offset)?, r#""9007199254740993""#);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Reports a non-canonical or out-of-range decimal wire integer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{type_name} must be a canonical unsigned decimal string")]
pub struct DecimalWireError {
    type_name: &'static str,
}

fn parse_decimal(value: &str, type_name: &'static str) -> Result<u64, DecimalWireError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_error| DecimalWireError { type_name })?;
    if parsed.to_string() == value {
        Ok(parsed)
    } else {
        Err(DecimalWireError { type_name })
    }
}

macro_rules! decimal_wire_type {
    ($(#[$docs:meta])* $name:ident, $export:literal) => {
        $(#[$docs])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        #[cfg_attr(feature = "ts", ts(export, export_to = $export, type = "string"))]
        pub struct $name(u64);

        impl $name {
            /// Creates a decimal wire value from its Rust integer.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the exact Rust integer value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Parses a canonical unsigned decimal string.
            ///
            /// # Errors
            ///
            /// Returns [`DecimalWireError`] for non-digits, overflow, empty
            /// strings, signs, or redundant leading zeroes.
            pub fn parse(value: &str) -> Result<Self, DecimalWireError> {
                parse_decimal(value, stringify!($name)).map(Self)
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                Display::fmt(&self.0, formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

decimal_wire_type!(
    /// Monotonic runtime generation scoped to one logical session.
    RuntimeGeneration,
    "RuntimeGeneration.ts"
);
decimal_wire_type!(
    /// Monotonic byte offset scoped to one PTY runtime.
    OutputOffset,
    "OutputOffset.ts"
);
decimal_wire_type!(
    /// Monotonic rendered-terminal revision.
    TerminalWatermark,
    "TerminalWatermark.ts"
);
decimal_wire_type!(
    /// Monotonic agent-activity revision scoped to one logical session.
    ActivityRevision,
    "ActivityRevision.ts"
);
decimal_wire_type!(
    /// Monotonic lifecycle-report sequence.
    ReportSequence,
    "ReportSequence.ts"
);
decimal_wire_type!(
    /// Kernel process-start identity paired with one PID.
    ProcessStartIdentity,
    "ProcessStartIdentity.ts"
);
