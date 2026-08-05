//! Defines validated worker runtime policy.

// Rust guideline compliant 2026-08-04

use std::time::Duration;

use pohunek_worker_protocol::MAX_DATA_PAYLOAD_BYTES;

/// Default bootstrap window before an uninitialized worker exits.
pub(crate) const DEFAULT_INITIALIZE_DEADLINE: Duration = Duration::from_secs(30);
/// Default raw-output history budget inherited from daemon-owned PTYs.
pub(crate) const DEFAULT_HISTORY_BYTES: usize = 10_000_000;
/// Default memory budget for one output subscriber.
pub(crate) const DEFAULT_SUBSCRIBER_BYTES: usize = 1_000_000;
/// Default maximum private-protocol data payload.
pub(crate) const DEFAULT_DATA_PAYLOAD_BYTES: usize = 64 * 1024;
/// Default maximum private-protocol JSON line.
pub(crate) const DEFAULT_CONTROL_LINE_BYTES: usize = 64 * 1024;
/// Default lifetime of a one-use data-stream token.
pub(crate) const DEFAULT_DATA_TOKEN_TTL: Duration = Duration::from_secs(10);
/// Default number of completed input plans retained for exact deduplication.
pub(crate) const DEFAULT_INPUT_DEDUP_ENTRIES: usize = 4_096;
/// Default graceful stop window before hard termination.
pub(crate) const DEFAULT_STOP_GRACE: Duration = Duration::from_millis(500);
/// Default final-screen retention while the daemon is unavailable.
pub(crate) const DEFAULT_TERMINAL_RETENTION: Duration = Duration::from_hours(24);
/// Default maximum time occupied by one stalled observation request.
///
/// Disconnect is observed on the dedicated stream, while this deliberately
/// short ceiling bounds peers that remain connected without making progress.
pub(crate) const DEFAULT_MAX_OBSERVATION_WAIT: Duration = Duration::from_secs(10);
/// Default maximum terminal rows serialized on the private control plane.
pub(crate) const DEFAULT_MAX_SNAPSHOT_ROWS: u16 = 512;
/// Default maximum terminal columns serialized on the private control plane.
pub(crate) const DEFAULT_MAX_SNAPSHOT_COLUMNS: u16 = 512;
/// Default maximum serialized terminal-snapshot control response.
pub(crate) const DEFAULT_MAX_SNAPSHOT_BYTES: usize = DEFAULT_CONTROL_LINE_BYTES;

/// Hard cap preventing one worker from consuming excessive output memory.
const MAX_HISTORY_BYTES: usize = 256 * 1024 * 1024;
/// Hard cap preventing one subscriber from consuming excessive output memory.
const MAX_SUBSCRIBER_BYTES: usize = 64 * 1024 * 1024;
/// Hard cap for a private control line.
const MAX_CONTROL_LINE_BYTES: usize = 1024 * 1024;
/// Hard cap for retained completed input plans.
const MAX_INPUT_DEDUP_ENTRIES: usize = 65_536;
/// Hard maximum keeps abandoned observation waits short and retry-oriented.
const MAX_OBSERVATION_WAIT: Duration = Duration::from_secs(10);

/// Invalid worker policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A byte budget is outside its safe range.
    #[error("{field} must be between 1 and {maximum} bytes, got {actual}")]
    Bytes {
        /// Configuration field name.
        field: &'static str,
        /// Rejected value.
        actual: usize,
        /// Inclusive upper bound.
        maximum: usize,
    },
    /// A count is outside its safe range.
    #[error("{field} must be between 1 and {maximum}, got {actual}")]
    Count {
        /// Configuration field name.
        field: &'static str,
        /// Rejected value.
        actual: usize,
        /// Inclusive upper bound.
        maximum: usize,
    },
    /// A duration must be nonzero.
    #[error("{field} must be greater than zero")]
    Duration {
        /// Configuration field name.
        field: &'static str,
    },
    /// A duration exceeds its safe maximum.
    #[error("{field} must not exceed {maximum:?}, got {actual:?}")]
    DurationMaximum {
        /// Configuration field name.
        field: &'static str,
        /// Rejected duration.
        actual: Duration,
        /// Inclusive upper bound.
        maximum: Duration,
    },
}

/// Frozen policy for one worker process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    /// Time allowed for the first valid initialization.
    pub initialize_deadline: Duration,
    /// Raw-output ring capacity.
    pub history_bytes: usize,
    /// Per-subscriber queued-output capacity.
    pub subscriber_bytes: usize,
    /// Maximum data-frame payload.
    pub data_payload_bytes: usize,
    /// Maximum control-protocol line.
    pub control_line_bytes: usize,
    /// One-use stream-token lifetime.
    pub data_token_ttl: Duration,
    /// Completed write-plan deduplication capacity.
    pub input_dedup_entries: usize,
    /// Grace period between soft and hard stop.
    pub stop_grace: Duration,
    /// Retention after terminal outcome when unacknowledged.
    pub terminal_retention: Duration,
    /// Maximum duration of one control-plane output wait.
    pub max_observation_wait: Duration,
    /// Maximum terminal rows returned by control-plane snapshots.
    pub max_snapshot_rows: u16,
    /// Maximum terminal columns returned by control-plane snapshots.
    pub max_snapshot_columns: u16,
    /// Maximum serialized terminal-snapshot control response bytes.
    pub max_snapshot_bytes: usize,
}

impl WorkerConfig {
    /// Creates the production worker policy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            initialize_deadline: DEFAULT_INITIALIZE_DEADLINE,
            history_bytes: DEFAULT_HISTORY_BYTES,
            subscriber_bytes: DEFAULT_SUBSCRIBER_BYTES,
            data_payload_bytes: DEFAULT_DATA_PAYLOAD_BYTES,
            control_line_bytes: DEFAULT_CONTROL_LINE_BYTES,
            data_token_ttl: DEFAULT_DATA_TOKEN_TTL,
            input_dedup_entries: DEFAULT_INPUT_DEDUP_ENTRIES,
            stop_grace: DEFAULT_STOP_GRACE,
            terminal_retention: DEFAULT_TERMINAL_RETENTION,
            max_observation_wait: DEFAULT_MAX_OBSERVATION_WAIT,
            max_snapshot_rows: DEFAULT_MAX_SNAPSHOT_ROWS,
            max_snapshot_columns: DEFAULT_MAX_SNAPSHOT_COLUMNS,
            max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }

    /// Validates every bounded worker policy value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a duration is zero or a bound is unsafe.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_duration("initialize_deadline", self.initialize_deadline)?;
        validate_bytes("history_bytes", self.history_bytes, MAX_HISTORY_BYTES)?;
        validate_bytes(
            "subscriber_bytes",
            self.subscriber_bytes,
            MAX_SUBSCRIBER_BYTES,
        )?;
        validate_bytes(
            "data_payload_bytes",
            self.data_payload_bytes,
            MAX_DATA_PAYLOAD_BYTES,
        )?;
        validate_bytes(
            "control_line_bytes",
            self.control_line_bytes,
            MAX_CONTROL_LINE_BYTES,
        )?;
        validate_duration("data_token_ttl", self.data_token_ttl)?;
        validate_count(
            "input_dedup_entries",
            self.input_dedup_entries,
            MAX_INPUT_DEDUP_ENTRIES,
        )?;
        validate_duration("stop_grace", self.stop_grace)?;
        validate_duration("terminal_retention", self.terminal_retention)?;
        validate_duration("max_observation_wait", self.max_observation_wait)?;
        if self.max_observation_wait > MAX_OBSERVATION_WAIT {
            return Err(ConfigError::DurationMaximum {
                field: "max_observation_wait",
                actual: self.max_observation_wait,
                maximum: MAX_OBSERVATION_WAIT,
            });
        }
        validate_count(
            "max_snapshot_rows",
            usize::from(self.max_snapshot_rows),
            usize::from(u16::MAX),
        )?;
        validate_count(
            "max_snapshot_columns",
            usize::from(self.max_snapshot_columns),
            usize::from(u16::MAX),
        )?;
        validate_bytes(
            "max_snapshot_bytes",
            self.max_snapshot_bytes,
            self.control_line_bytes,
        )
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_bytes(field: &'static str, actual: usize, maximum: usize) -> Result<(), ConfigError> {
    if actual == 0 || actual > maximum {
        return Err(ConfigError::Bytes {
            field,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn validate_count(field: &'static str, actual: usize, maximum: usize) -> Result<(), ConfigError> {
    if actual == 0 || actual > maximum {
        return Err(ConfigError::Count {
            field,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn validate_duration(field: &'static str, value: Duration) -> Result<(), ConfigError> {
    if value.is_zero() {
        return Err(ConfigError::Duration { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, WorkerConfig};
    use pohunek_worker_protocol::MAX_DATA_PAYLOAD_BYTES;
    use std::time::Duration;

    #[test]
    fn defaults_are_valid() {
        WorkerConfig::new().validate().expect("valid defaults");
    }

    #[test]
    fn zero_duration_is_rejected() {
        let config = WorkerConfig {
            stop_grace: Duration::ZERO,
            ..WorkerConfig::new()
        };

        assert_eq!(
            config.validate(),
            Err(ConfigError::Duration {
                field: "stop_grace"
            })
        );
    }

    #[test]
    fn zero_memory_budget_is_rejected() {
        let config = WorkerConfig {
            subscriber_bytes: 0,
            ..WorkerConfig::new()
        };

        assert!(matches!(
            config.validate(),
            Err(ConfigError::Bytes {
                field: "subscriber_bytes",
                ..
            })
        ));
    }

    #[test]
    fn data_payload_cannot_exceed_wire_limit() {
        let config = WorkerConfig {
            data_payload_bytes: MAX_DATA_PAYLOAD_BYTES + 1,
            ..WorkerConfig::new()
        };

        assert_eq!(
            config.validate(),
            Err(ConfigError::Bytes {
                field: "data_payload_bytes",
                actual: MAX_DATA_PAYLOAD_BYTES + 1,
                maximum: MAX_DATA_PAYLOAD_BYTES,
            })
        );
    }

    #[test]
    fn observation_wait_is_short_and_bounded() {
        let exact = WorkerConfig {
            max_observation_wait: Duration::from_secs(10),
            ..WorkerConfig::new()
        };
        exact.validate().expect("exact wait maximum");

        let above = WorkerConfig {
            max_observation_wait: Duration::from_secs(10) + Duration::from_millis(1),
            ..WorkerConfig::new()
        };
        assert!(matches!(
            above.validate(),
            Err(ConfigError::DurationMaximum {
                field: "max_observation_wait",
                ..
            })
        ));
    }

    #[test]
    fn snapshot_bounds_are_nonzero() {
        let config = WorkerConfig {
            max_snapshot_rows: 0,
            ..WorkerConfig::new()
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Count {
                field: "max_snapshot_rows",
                ..
            })
        ));
    }
}
