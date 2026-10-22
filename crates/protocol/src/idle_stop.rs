//! Wire types of the idle-stop default behind `session.idle_stop.get` and
//! `session.idle_stop.set`, and the session metadata key that opts one session in.
//!
//! A session is stopped for idleness only when its metadata carries
//! [`IDLE_STOP_METADATA_KEY`]. The value is [`IDLE_STOP_DEFAULT_VALUE`], which
//! takes the host default held here, or a decimal number of seconds that
//! overrides it. The default is a separate record from the retention policy, so
//! the retention policy document keeps its shape for every reader of it.

// Rust guideline compliant 2026-10-22

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Session metadata key that opts a session into idle stop.
pub const IDLE_STOP_METADATA_KEY: &str = "pohunek.idle_stop_after_secs";

/// Metadata value that takes the host default instead of a per-session duration.
pub const IDLE_STOP_DEFAULT_VALUE: &str = "default";

/// Reports a value of [`IDLE_STOP_METADATA_KEY`] that opts in nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdleStopValueError {
    /// The value is neither `default` nor a plain decimal number of seconds.
    #[error("pohunek.idle_stop_after_secs must be \"default\" or a decimal number of seconds")]
    Malformed,
    /// The duration is below the host minimum.
    #[error("pohunek.idle_stop_after_secs must be at least {minimum} seconds")]
    BelowMinimum {
        /// Smallest accepted duration in seconds.
        minimum: u32,
    },
}

/// What a session's [`IDLE_STOP_METADATA_KEY`] value asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleStopOptIn {
    /// Stop after the host default, when one is set.
    HostDefault,
    /// Stop after this many seconds of idleness.
    Seconds(u32),
}

impl IdleStopOptIn {
    /// Parses a metadata value; durations below `minimum` are refused.
    ///
    /// Only ASCII digits are accepted, so signs, whitespace, exponents and unit
    /// suffixes are all malformed.
    ///
    /// # Errors
    ///
    /// Returns [`IdleStopValueError`] for a malformed value or a duration below
    /// `minimum`.
    pub fn parse(value: &str, minimum: u32) -> Result<Self, IdleStopValueError> {
        if value == IDLE_STOP_DEFAULT_VALUE {
            return Ok(Self::HostDefault);
        }
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(IdleStopValueError::Malformed);
        }
        let seconds: u32 = value
            .parse()
            .map_err(|_overflow| IdleStopValueError::Malformed)?;
        if seconds < minimum {
            return Err(IdleStopValueError::BelowMinimum { minimum });
        }
        Ok(Self::Seconds(seconds))
    }
}

/// Parameters of `session.idle_stop.set`.
///
/// The default is replaced whole: an omitted or `null` `default_after_secs`, and
/// `0`, clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionIdleStopParams.ts"))]
#[serde(deny_unknown_fields)]
pub struct SessionIdleStopParams {
    /// Idle seconds after which a session opted in with `default` is stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default_after_secs: Option<u32>,
}

/// Result of `session.idle_stop.get` and `session.idle_stop.set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SessionIdleStopResult.ts"))]
pub struct SessionIdleStopResult {
    /// Idle seconds after which a session opted in with `default` is stopped;
    /// absent when no default is set, so such a session is never stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default_after_secs: Option<u32>,
    /// Smallest duration the host accepts, for the default and for a session's
    /// own value.
    pub min_after_secs: u32,
}
