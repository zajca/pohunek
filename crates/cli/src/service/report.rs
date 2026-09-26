//! Serializable results of `pohunek service` operations.
//!
//! These types are the stable `--json` payloads (inside the CLI's usual
//! `{cli_version, protocol, ok}` envelope). Fields are only ever added.

// Rust guideline compliant 2026-09-25

use std::path::PathBuf;

use pohunek_platform::supervisor::{ServiceObservation, ServiceState};
use serde::Serialize;

use super::usage::JournalRef;

/// Result of `pohunek service install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReport {
    /// The installed version.
    pub version: String,
    /// The installation prefix.
    pub prefix: PathBuf,
    /// The installation namespace.
    pub namespace: String,
    /// The written `service.toml`.
    pub config_path: PathBuf,
    /// The daemon's versioned executable.
    pub daemon_executable: PathBuf,
    /// Whether an interrupted install of the same version was resumed.
    pub resumed: bool,
    /// An unrelated interrupted transaction that was rolled back first.
    pub rolled_back: Option<PendingReport>,
}

/// Result of `pohunek service upgrade`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradeReport {
    /// The version active before the upgrade.
    pub from_version: String,
    /// The version active after the upgrade.
    pub to_version: String,
    /// Whether the requested version was already active, so nothing restarted.
    pub unchanged: bool,
    /// Whether an interrupted upgrade to the same version was resumed.
    pub resumed: bool,
    /// An unrelated interrupted transaction that was rolled back first.
    pub rolled_back: Option<PendingReport>,
    /// Version directories removed by garbage collection.
    pub removed_versions: Vec<String>,
    /// Version directories kept, with the reason.
    pub kept_versions: Vec<KeptVersion>,
    /// Why garbage collection did not run, when it failed.
    pub gc_error: Option<String>,
}

/// Result of `pohunek service uninstall`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UninstallReport {
    /// Sessions stopped because `--stop-sessions` was given.
    pub stopped_sessions: Vec<String>,
    /// Worker jobs retired after their sessions ended.
    pub retired_workers: Vec<String>,
    /// Files and directories removed.
    pub removed: Vec<PathBuf>,
    /// Version directories kept, with the reason.
    pub kept_versions: Vec<KeptVersion>,
    /// Whether durable metadata was purged.
    pub purged: bool,
    /// Whether the daemon had to be started to enumerate sessions.
    pub started_daemon: bool,
    /// An interrupted transaction that was rolled back first.
    pub rolled_back: Option<PendingReport>,
}

/// A version directory garbage collection kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptVersion {
    /// The kept version.
    pub version: String,
    /// Why it is still needed.
    pub reason: String,
}

/// An interrupted install or upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingReport {
    /// `install` or `upgrade`.
    pub operation: &'static str,
    /// The version it was installing.
    pub version: String,
    /// The last completed step.
    pub step: &'static str,
}

/// Result of `pohunek service status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// Whether `service.toml` exists.
    pub installed: bool,
    /// Where `service.toml` lives.
    pub config_path: PathBuf,
    /// The installation namespace.
    pub namespace: Option<String>,
    /// The installation prefix.
    pub prefix: Option<PathBuf>,
    /// The active version.
    pub active_version: Option<String>,
    /// The daemon job, when the service manager knows it.
    pub daemon: Option<JobReport>,
    /// Why the daemon job could not be observed, when it could not.
    pub daemon_error: Option<String>,
    /// Installed version directories.
    pub versions: Vec<VersionReport>,
    /// Worker jobs of this namespace, one per generation.
    pub workers: Vec<WorkerReport>,
    /// Why worker jobs could not be discovered, when they could not.
    pub workers_error: Option<String>,
    /// Journal files that could not be read.
    pub unreadable_journals: Vec<PathBuf>,
    /// An interrupted install or upgrade awaiting resume or rollback.
    pub pending_transaction: Option<PendingReport>,
    /// Whether another `pohunek service` command is running right now; its
    /// record is then in flight rather than interrupted.
    pub transaction_in_progress: bool,
}

/// One service-manager job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobReport {
    /// The backend-neutral service ID.
    pub service_id: String,
    /// `starting`, `running`, `stopping`, `stopped`, `failed`, or `unknown`.
    pub state: &'static str,
    /// The main process ID, when one runs.
    pub pid: Option<u32>,
    /// The executable the backend proved, when it could.
    pub executable: Option<PathBuf>,
    /// The arguments the backend proved, when it could.
    pub arguments: Option<Vec<String>>,
}

impl From<&ServiceObservation> for JobReport {
    fn from(observation: &ServiceObservation) -> Self {
        Self {
            service_id: observation.id.to_string(),
            state: state_name(observation.state),
            pid: observation.process.map(|process| process.pid),
            executable: observation
                .definition
                .as_ref()
                .map(|facts| facts.executable.clone()),
            arguments: observation
                .definition
                .as_ref()
                .map(|facts| facts.arguments.clone()),
        }
    }
}

/// One worker generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerReport {
    /// The service-manager job.
    #[serde(flatten)]
    pub job: JobReport,
    /// Logical session ID.
    pub session_id: String,
    /// Worker generation.
    pub generation: String,
    /// The installed version the worker runs, when it runs one.
    pub version: Option<String>,
}

/// One installed version directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionReport {
    /// The version.
    pub version: String,
    /// Whether `service.toml` names it.
    pub active: bool,
    /// Journals of workers that may still run from it.
    pub journals: Vec<JournalRef>,
    /// Running processes executing from it.
    pub pids: Vec<u32>,
}

/// Returns the stable lowercase name of a service state.
#[must_use]
pub fn state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Starting => "starting",
        ServiceState::Running => "running",
        ServiceState::Stopping => "stopping",
        ServiceState::Stopped => "stopped",
        ServiceState::Failed => "failed",
        ServiceState::Unknown => "unknown",
    }
}
