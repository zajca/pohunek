//! Serializable results of `pohunek service` operations.
//!
//! These types are the stable `--json` payloads (inside the CLI's usual
//! `{cli_version, protocol, ok}` envelope). Fields are only ever added.

// Rust guideline compliant 2026-09-30

use std::path::{Path, PathBuf};

use pohunek_platform::supervisor::{ServiceObservation, ServiceState};
use pohunek_service_config::preflight::{PreflightReport, SessionVerdict};
use pohunek_service_config::runtimes::EnsureReport;
use serde::ser::SerializeSeq as _;
use serde::{Serialize, Serializer};

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
    /// How the daemon job's executable search path was obtained.
    pub search_path: SearchPathReport,
    /// The official runtime step the install ran: the plan, or the apply when
    /// the install installed packages or pinned profiles. The install applies
    /// the runtimes its profiles, sessions and integrations use, and nothing
    /// else.
    #[serde(default)]
    pub runtimes: Option<EnsureReport>,
    /// What the run did to the version directory; `null` when `--repair` was
    /// not given and nothing was restored.
    #[serde(default)]
    pub repair: Option<RepairReport>,
}

/// What `--repair` did to the version directory of the release it installs,
/// and to the daemon job running from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairReport {
    /// The version whose directory was repaired.
    pub version: String,
    /// Whether the release's copy was published because the directory was
    /// missing or refused.
    pub restored: bool,
    /// Where the refused entry was moved, kept for the operator to inspect and
    /// delete; `null` when none existed.
    pub set_aside: Option<PathBuf>,
    /// What happened to the daemon job: `none`, `installed` (the job was
    /// missing), `started` (it was stopped), `restarted` (it was running) or
    /// `kept` (a resumed transaction had already replaced the daemon, which
    /// still runs from the entry that was moved aside).
    pub daemon: &'static str,
}

/// Result of `pohunek service refresh-path`.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the serialized report exposes independent facts about the run"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefreshPathReport {
    /// Whether the command only previewed the change (`--dry-run`).
    pub dry_run: bool,
    /// Whether the discovered path differs from the recorded one.
    pub changed: bool,
    /// Whether the refresh took effect: `service.toml` records the new path and
    /// the daemon job runs with it.
    pub applied: bool,
    /// The directories recorded before, in search order.
    pub recorded: Vec<PathBuf>,
    /// The directories the login shell reports now, in search order; for a
    /// resumed refresh, the directories the interrupted run journaled.
    pub discovered: Vec<PathBuf>,
    /// Discovered directories the recorded path lacks.
    pub added: Vec<PathBuf>,
    /// Recorded directories the discovered path lacks.
    pub removed: Vec<PathBuf>,
    /// The login shell that was tried.
    pub shell_used: Option<PathBuf>,
    /// Whether `shell_used` is the built-in default because `$SHELL` was unset.
    pub shell_defaulted: bool,
    /// Existing directories refused as untrusted.
    pub dropped: Vec<DroppedPath>,
    /// What happened to the daemon job: `restarted`, `would_restart` (a dry
    /// run of a change), or `none`.
    pub daemon_restart: &'static str,
    /// Whether the daemon job was registered but not running, so the
    /// replacement started it (or would).
    pub daemon_started: bool,
    /// Worker jobs the restart leaves running; `None` when they could not be
    /// listed.
    pub workers_kept: Option<usize>,
    /// Whether an interrupted refresh was resumed.
    pub resumed: bool,
    /// An interrupted refresh that was rolled back instead of resumed.
    pub rolled_back: Option<PendingReport>,
    /// The new daemon's adoption verdict the refresh passed (or, for a dry
    /// run, would be judged by); `null` when no verdict was needed.
    pub preflight: Option<PreflightReport>,
    /// Whether `--accept-runtime-loss` accepted at-risk sessions in the verdict.
    pub accepted_runtime_loss: bool,
    /// Conditions the operator should know, such as keys of `service.toml`
    /// this version does not know.
    pub warnings: Vec<String>,
}

/// How the daemon job's executable search path was obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchPathReport {
    /// `login_shell`, `fallback`, `recorded` (an interrupted install resumed
    /// with the path already written to `service.toml`), or `unmanaged` (the
    /// service manager's own `PATH` applies and none is recorded).
    pub source: &'static str,
    /// The recorded directories in search order.
    pub entries: Vec<PathBuf>,
    /// The login shell tried, when discovery ran.
    pub shell_used: Option<PathBuf>,
    /// Whether `shell_used` is the built-in default because `$SHELL` was unset.
    pub shell_defaulted: bool,
    /// Why login-shell discovery failed, when the fallback list was used.
    pub login_shell_failure: Option<String>,
    /// Existing directories refused as untrusted.
    pub dropped: Vec<DroppedPath>,
}

impl SearchPathReport {
    /// Whether the operator should be warned: the login shell did not supply
    /// the path, or a directory was refused.
    #[must_use]
    pub fn needs_warning(&self) -> bool {
        self.login_shell_failure.is_some() || !self.dropped.is_empty()
    }
}

/// A directory refused when the search path was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedPath {
    /// The directory as listed.
    pub path: String,
    /// Why it was refused.
    pub reason: &'static str,
}

/// Result of `pohunek service upgrade`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradeReport {
    /// Conditions of the upgrade's inputs that the operator must be told.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The version active before the upgrade.
    pub from_version: String,
    /// The version active after the upgrade.
    pub to_version: String,
    /// Whether the requested version was already active, so the active version
    /// did not change. Only `--repair` or a restored directory restarts the
    /// daemon then.
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
    /// The requested daemon's adoption preflight, or the previous daemon's
    /// rollback verdict when the operation only undid an interrupted upgrade.
    /// `null` when no daemon replacement needed a verdict.
    pub preflight: Option<PreflightReport>,
    /// Whether `--accept-runtime-loss` accepted at-risk sessions in the
    /// upgrade or a preceding rollback.
    pub accepted_runtime_loss: bool,
    /// How the at-risk sessions of the final gate were accepted.
    #[serde(flatten)]
    pub acceptance: AcceptanceGroups,
    /// The `service.toml` schema the transaction migrates from, when it does.
    ///
    /// A migration stops the daemon before it rewrites `service.toml` and
    /// restores the exact pre-upgrade bytes on rollback.
    pub config_schema_migration: Option<u32>,
    /// The official runtime report: the plan, or the apply when this run
    /// applied it. `null` when the step did not run in this invocation.
    #[serde(default)]
    pub runtimes: Option<EnsureReport>,
    /// The integration hooks reinstalled once the new daemon was ready. A
    /// failure is reported here and never rolls the upgrade back.
    #[serde(default)]
    pub integrations: Vec<IntegrationReinstall>,
    /// What the run did to the version directory and the daemon job; `null`
    /// when `--repair` was not given and nothing was restored or registered.
    #[serde(default)]
    pub repair: Option<RepairReport>,
    /// Entries earlier repairs moved aside; only the operator deletes them.
    ///
    /// Serialized as byte-escaped display strings, as the retained purge
    /// entries are.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_purge_paths"
    )]
    pub refused_entries: Vec<PathBuf>,
}

/// One integration hook reinstalled after an upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationReinstall {
    /// The integration: a runtime id, or `hermes` with its profile.
    pub integration: String,
    /// `installed`, `failed`, or `gap`. A gap is a Hermes home without the
    /// pohunek marker, which the upgrade does not write into, or a host
    /// without a Hermes home, which is skipped.
    pub status: &'static str,
    /// The failure, the gap's command or skip note, or `null` when installed.
    pub detail: Option<String>,
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
    /// Retained data/migration entries, serialized as byte-escaped display strings.
    ///
    /// Arbitrary Unix names must not make JSON rendering fail after purge commits.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_purge_paths"
    )]
    pub kept_purge_entries: Vec<PathBuf>,
    /// Version directories kept, with the reason.
    pub kept_versions: Vec<KeptVersion>,
    /// Whether durable metadata was purged.
    pub purged: bool,
    /// Whether the daemon had to be started to enumerate sessions.
    pub started_daemon: bool,
    /// An interrupted transaction that was rolled back first.
    pub rolled_back: Option<PendingReport>,
    /// Entries earlier repairs moved aside and the uninstall left in place;
    /// only the operator deletes them.
    ///
    /// Serialized as byte-escaped display strings, as the retained purge
    /// entries are.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_purge_paths"
    )]
    pub refused_entries: Vec<PathBuf>,
}

/// Display a retained path without losing invalid UTF-8 bytes or emitting controls.
pub(crate) fn escaped_purge_path(path: &Path) -> String {
    path.as_os_str()
        .as_encoded_bytes()
        .escape_ascii()
        .to_string()
}

fn serialize_purge_paths<S: Serializer>(
    paths: &[PathBuf],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut sequence = serializer.serialize_seq(Some(paths.len()))?;
    for path in paths {
        sequence.serialize_element(&escaped_purge_path(path))?;
    }
    sequence.end()
}

/// Result of a passing `pohunek service check`.
///
/// A failing check returns the error the install or upgrade would return
/// instead, with the same code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    /// The transaction the check covered: `install` or `upgrade`.
    pub operation: &'static str,
    /// The version that transaction would install.
    pub version: String,
    /// The installation prefix it would use.
    pub prefix: PathBuf,
    /// The installation namespace.
    pub namespace: String,
    /// The `service.toml` it would write or upgrade.
    pub config_path: PathBuf,
    /// The interrupted transaction it would resume or roll back first.
    pub pending_transaction: Option<PendingReport>,
    /// What it would do with `pending_transaction`: `resume` or `roll_back`.
    pub pending_action: Option<&'static str>,
    /// Whether the check ran under a transaction lock a `pohunek service
    /// lock` ancestor holds, so no other transaction can change the result.
    pub locked: bool,
    /// The requested daemon's adoption preflight, or the previous daemon's
    /// rollback verdict when the check only covers an interrupted upgrade.
    /// `null` when no daemon replacement needs a verdict.
    pub preflight: Option<PreflightReport>,
    /// Whether `--accept-runtime-loss` would accept at-risk sessions in the
    /// upgrade or a preceding rollback.
    pub accepted_runtime_loss: bool,
    /// How the at-risk sessions of the checked gate would be accepted.
    #[serde(flatten)]
    pub acceptance: AcceptanceGroups,
    /// The `service.toml` schema the checked transaction would migrate from,
    /// when it would.
    ///
    /// A migration stops the daemon before it rewrites `service.toml` and
    /// restores the exact pre-upgrade bytes on rollback.
    pub config_schema_migration: Option<u32>,
    /// Conditions of the check's inputs that the operator must be told, such as
    /// a service manager whose environment was not read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// How the at-risk verdicts of an adoption gate were, or would be, accepted.
///
/// Every group is empty and omitted unless the gate had at-risk verdicts of
/// that kind. `accepted_runtime_loss` keeps meaning that the flag accepted a
/// remainder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AcceptanceGroups {
    /// At-risk verdicts an earlier upgrade accepted and the daemon still
    /// reports with the identical scope; they do not block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previously_accepted: Vec<SessionVerdict>,
    /// The version of the upgrade that accepted them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previously_accepted_at: Option<String>,
    /// Verdicts the flag accepts now, which the completed upgrade records so
    /// the identical risk is not asked about again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub durable_acceptance: Vec<SessionVerdict>,
    /// Verdicts the flag accepts now that no scope can bind, so the next
    /// upgrade asks about them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub one_time_acceptance: Vec<SessionVerdict>,
}

/// A version directory garbage collection kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptVersion {
    /// The kept version.
    pub version: String,
    /// Why it is still needed.
    pub reason: String,
}

/// An interrupted install, upgrade, or search-path refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingReport {
    /// `install`, `upgrade`, or `refresh_path`.
    pub operation: &'static str,
    /// The version it was installing.
    pub version: String,
    /// The last completed step.
    pub step: &'static str,
    /// Whether its rollback has begun; shown only when it has.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rolling_back: bool,
    /// The previous daemon's read-only verdict when rollback replaced it.
    pub preflight: Option<PreflightReport>,
    /// Whether the operator accepted the rollback's at-risk sessions.
    pub accepted_runtime_loss: bool,
    /// Conditions of the journal that the rollback handles with a fallback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
    /// Conditions of the installation that the operator must be told, such as
    /// keys of `service.toml` this version does not know.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
