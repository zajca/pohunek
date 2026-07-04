//! Typed payloads for daemon-side doctor checks.

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    /// Check passed.
    Ok,
    /// Check passed with a warning.
    Warn,
    /// Check failed.
    Fail,
}

impl DoctorStatus {
    /// Human-readable compact status label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One reported doctor check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Short check name.
    pub name: String,
    /// Outcome status for this check.
    pub status: DoctorStatus,
    /// Human-readable detail explaining the outcome.
    pub detail: String,
}

impl DoctorCheck {
    #[must_use]
    pub fn new(name: impl Into<String>, status: DoctorStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }
}

/// Aggregated doctor report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Individual checks that were run.
    pub checks: Vec<DoctorCheck>,
    /// Aggregated overall status across all checks.
    pub overall: DoctorStatus,
}

impl DoctorReport {
    #[must_use]
    pub fn from_checks(checks: Vec<DoctorCheck>) -> Self {
        let overall = if checks
            .iter()
            .any(|check| check.status == DoctorStatus::Fail)
        {
            DoctorStatus::Fail
        } else {
            DoctorStatus::Ok
        };
        Self { checks, overall }
    }
}

/// Result returned by `daemon.health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHealthResult {
    /// Liveness status. Current daemons return `"ok"`.
    pub status: String,
    /// Daemon build version.
    pub daemon_version: String,
    /// Protocol version spoken by the daemon.
    pub protocol_version: ProtocolVersion,
}

/// Result returned by `daemon.doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDoctorResult {
    /// Full doctor report produced by the daemon.
    pub report: DoctorReport,
}
