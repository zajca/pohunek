//! Typed payloads for daemon-side doctor checks.

use serde::{Deserialize, Serialize};

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn,
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
    pub name: String,
    pub status: DoctorStatus,
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
    pub checks: Vec<DoctorCheck>,
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

/// Result returned by `daemon.doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDoctorResult {
    pub report: DoctorReport,
}
