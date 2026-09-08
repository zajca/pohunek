//! Typed payloads for daemon-side doctor checks.

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "DoctorStatus.ts"))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "DoctorCheck.ts"))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "DoctorReport.ts"))]
pub struct DoctorReport {
    /// Individual checks that were run.
    pub checks: Vec<DoctorCheck>,
    /// Aggregated overall status across all checks.
    pub overall: DoctorStatus,
}

impl DoctorReport {
    /// Aggregate checks by their highest severity.
    ///
    /// An empty report and an all-`Ok` report are `Ok`. A `Warn` is retained
    /// unless any check is `Fail`, which always takes precedence.
    #[must_use]
    pub fn from_checks(checks: Vec<DoctorCheck>) -> Self {
        let overall = checks.iter().fold(DoctorStatus::Ok, |overall, check| {
            match (overall, check.status) {
                (DoctorStatus::Fail, _) | (_, DoctorStatus::Fail) => DoctorStatus::Fail,
                (DoctorStatus::Warn, _) | (_, DoctorStatus::Warn) => DoctorStatus::Warn,
                (DoctorStatus::Ok, DoctorStatus::Ok) => DoctorStatus::Ok,
            }
        });
        Self { checks, overall }
    }
}

#[cfg(test)]
mod tests {
    use super::{DoctorCheck, DoctorReport, DoctorStatus};

    #[test]
    fn aggregates_highest_check_severity() {
        let cases = [
            ("empty", Vec::new(), DoctorStatus::Ok),
            ("ok", vec![DoctorStatus::Ok], DoctorStatus::Ok),
            ("warn", vec![DoctorStatus::Warn], DoctorStatus::Warn),
            ("fail", vec![DoctorStatus::Fail], DoctorStatus::Fail),
            (
                "warn and fail",
                vec![DoctorStatus::Warn, DoctorStatus::Fail],
                DoctorStatus::Fail,
            ),
        ];

        for (name, statuses, expected) in cases {
            let checks = statuses
                .into_iter()
                .enumerate()
                .map(|(index, status)| DoctorCheck::new(format!("check-{index}"), status, name))
                .collect();
            let report = DoctorReport::from_checks(checks);
            assert_eq!(report.overall, expected, "case {name}");
        }
    }
}

/// Result returned by `daemon.health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "DaemonHealthResult.ts"))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "DaemonDoctorResult.ts"))]
pub struct DaemonDoctorResult {
    /// Full doctor report produced by the daemon.
    pub report: DoctorReport,
}
