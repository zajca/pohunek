//! Read-only compatibility judgment before restoring a previous daemon.
//!
//! Releases with `upgrade-preflight` judge their own reader and worker
//! adoption. The two v0.33 releases have no preflight and can silently skip
//! records they cannot load, so only their own schema-1 store is admitted:
//! every line is validated against the frozen typed reader of the exact
//! previous release ([`super::legacy_store`], selected by
//! [`super::legacy_store::LegacyReader`]), so a record that reader would
//! drop, and a whole-store rewrite would lose, refuses the rollback. The two
//! releases' readers differ, so their verdicts are frozen separately and a
//! line one keeps never proves another's contract. Unproven live workers
//! need explicit runtime-loss consent.

// Rust guideline compliant 2026-10-09

use pohunek_paths::{InstallLayout, DAEMON_EXECUTABLE_NAME, METADATA_STORE_NAME};
use pohunek_platform::filesystem::{EntryKind, TrustedDir};
use pohunek_platform::process::ProcessInspector;
use pohunek_platform::supervisor::WorkerKey;
use pohunek_service_config::preflight::{
    PreflightReport, SessionVerdict, StoreReport, StoreState, Verdict, REPORT_VERSION,
};
use std::collections::BTreeSet;
use std::path::Path;

use super::backend::Backend;
use super::context::Context;
use super::engine::job_alive;
use super::error::Error;
use super::layout;
use super::legacy_store;
use super::legacy_store::LegacyReader;
use super::preflight::{self, AdoptionPreflight, Gated};
use super::settings;
use super::usage::Journals;

/// The legacy metadata layout; the current daemon stamps later schemas.
const LEGACY_STORE_SCHEMA: u32 = 1;
/// Mode of the owner-private metadata store.
const STORE_MODE: u32 = 0o600;
/// Mode of the owner-private metadata directory.
const DATA_MODE: u32 = 0o700;

/// Dependencies needed to judge the previous daemon without changing state.
pub(super) struct Inputs<'a> {
    pub preflight: &'a dyn AdoptionPreflight,
    pub context: &'a Context,
    pub backend: Option<&'a Backend>,
    pub layout: &'a InstallLayout,
    pub inspector: &'a dyn ProcessInspector,
}

/// Judges the reader and runtime adoption of `previous` before rollback.
///
/// # Errors
///
/// Returns a typed rollback refusal for an unreadable store, at-risk live
/// workers without consent, or a previous daemon that cannot judge itself.
pub(super) async fn gate(
    inputs: Inputs<'_>,
    daemon_dir: &Path,
    previous: &str,
    reported_version: &str,
    accept_runtime_loss: bool,
) -> Result<Gated, Error> {
    let judged = if let Some(reader) = LegacyReader::from_version(previous) {
        let daemon = daemon_dir.join(DAEMON_EXECUTABLE_NAME);
        preflight::probe_daemon(&daemon, reported_version)
            .await
            .map_err(|error| Error::RollbackPreflightFailed {
                version: previous.to_owned(),
                detail: error.to_string(),
            })?;
        let report = legacy_report(&inputs, previous, reader).await?;
        preflight::decide(report, accept_runtime_loss)
    } else {
        preflight::gate(
            inputs.preflight,
            inputs.context,
            daemon_dir,
            reported_version,
            accept_runtime_loss,
        )
        .await
    };
    judged.map_err(|error| match error {
        Error::UpgradeStoreUnusable { path, code, detail } => Error::RollbackStoreUnusable {
            version: previous.to_owned(),
            path,
            code,
            detail,
        },
        Error::UpgradeAtRisk { sessions } => Error::RollbackAtRisk {
            version: previous.to_owned(),
            sessions,
        },
        other => Error::RollbackPreflightFailed {
            version: previous.to_owned(),
            detail: other.to_string(),
        },
    })
}

async fn legacy_report(
    inputs: &Inputs<'_>,
    previous: &str,
    reader: LegacyReader,
) -> Result<PreflightReport, Error> {
    let store = store_report(inputs.context, reader);
    let sessions = if store.state == StoreState::Refused {
        Vec::new()
    } else {
        legacy_workers(inputs, previous).await?
    };
    Ok(PreflightReport {
        report_version: REPORT_VERSION,
        daemon_version: previous.to_owned(),
        store,
        sessions,
        unmanaged_workers: Vec::new(),
    })
}

async fn legacy_workers(inputs: &Inputs<'_>, previous: &str) -> Result<Vec<SessionVerdict>, Error> {
    let backend = inputs
        .backend
        .ok_or_else(|| Error::RollbackPreflightFailed {
            version: previous.to_owned(),
            detail: "worker job inventory is unavailable".to_owned(),
        })?;
    let mut sessions = Vec::new();
    let jobs = backend.workers().discover_strict().await.map_err(|error| {
        Error::RollbackPreflightFailed {
            version: previous.to_owned(),
            detail: format!("cannot inspect worker jobs: {error}"),
        }
    })?;
    let mut covered = BTreeSet::new();
    let mut covered_pids = BTreeSet::new();
    for job in jobs {
        if !job_alive(&job) {
            continue;
        }
        if let Some(process) = job.process {
            covered_pids.insert(process.pid);
        }
        let (session_id, generation) = WorkerKey::from_service_id(&job.id).map_or_else(
            |_| (job.id.to_string(), String::new()),
            |key| (key.session_id().to_owned(), key.generation().to_owned()),
        );
        covered.insert((session_id.clone(), generation));
        sessions.push(unverified(
            session_id,
            format!(
                "the previous daemon has no adoption preflight for worker job {}",
                job.id
            ),
        ));
    }
    let journals =
        Journals::scan(inputs.context.paths()).map_err(|error| Error::RollbackPreflightFailed {
            version: previous.to_owned(),
            detail: format!("cannot inspect worker journals: {error}"),
        })?;
    for journal in journals.live(inputs.inspector) {
        if let Some(worker) = journal.worker {
            covered_pids.insert(worker.pid);
        }
        if covered.insert((journal.session_id.clone(), journal.generation.clone())) {
            sessions.push(unverified(
                journal.session_id.clone(),
                format!(
                    "the previous daemon has no adoption preflight for worker {} generation {}",
                    journal.worker_id, journal.generation
                ),
            ));
        }
    }
    for journal in journals.outdated_live(inputs.inspector) {
        sessions.push(unverified(
            journal.path.display().to_string(),
            "the worker journal cannot be judged by this installer".to_owned(),
        ));
    }
    for path in journals.unreadable {
        sessions.push(unverified(
            path.display().to_string(),
            "the worker journal is unreadable".to_owned(),
        ));
    }
    sessions.extend(unattributed_workers(inputs, previous, &covered_pids)?);
    sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    Ok(sessions)
}

fn unattributed_workers(
    inputs: &Inputs<'_>,
    previous: &str,
    covered_pids: &BTreeSet<u32>,
) -> Result<Vec<SessionVerdict>, Error> {
    let mut sessions = Vec::new();
    let processes =
        inputs
            .inspector
            .same_user_processes()
            .map_err(|error| Error::RollbackPreflightFailed {
                version: previous.to_owned(),
                detail: format!("cannot inspect worker processes: {error}"),
            })?;
    for process in processes {
        let candidate = process.comm.starts_with("pohunek-sess")
            || process.cmdline.first().is_some_and(|arg| {
                Path::new(arg)
                    .file_name()
                    .is_some_and(|name| name == "pohunek-sessiond")
            });
        if !candidate || covered_pids.contains(&process.pid) {
            continue;
        }
        let executable = match inputs.inspector.executable(process.pid) {
            Ok(Some(executable)) => executable,
            Ok(None) => continue,
            Err(error) if error.is_race() => continue,
            Err(error) => {
                return Err(Error::RollbackPreflightFailed {
                    version: previous.to_owned(),
                    detail: format!("cannot inspect executable of pid {}: {error}", process.pid),
                });
            }
        };
        if layout::version_of(inputs.layout, &executable).is_some() {
            sessions.push(unverified(
                format!("pid-{}", process.pid),
                format!(
                    "unattributed live worker pid {} may lose recovery",
                    process.pid
                ),
            ));
        }
    }
    Ok(sessions)
}

fn unverified(session_id: String, detail: String) -> SessionVerdict {
    SessionVerdict {
        session_id,
        name: None,
        verdict: Verdict::WouldNotBeAdopted,
        code: "legacy_worker_adoption_unverified".to_owned(),
        detail,
    }
}

fn store_report(context: &Context, reader: LegacyReader) -> StoreReport {
    let mut report = StoreReport {
        path: context.paths().data_dir.join(METADATA_STORE_NAME),
        state: StoreState::Missing,
        schema_from: None,
        schema_to: LEGACY_STORE_SCHEMA,
        records: 0,
        error_code: None,
        error: None,
    };
    let bytes = match read_legacy_store(context) {
        Ok(None) => return report,
        Ok(Some(bytes)) => bytes,
        Err(detail) => return refused(report, "legacy_store_unreadable", detail),
    };
    match legacy_store::validate_records(&bytes, reader) {
        Ok(records) => {
            report.state = StoreState::UpToDate;
            report.records = records;
            if records > 0 {
                report.schema_from = Some(LEGACY_STORE_SCHEMA);
            }
            report
        }
        Err((code, detail)) => refused(report, code, detail),
    }
}

fn refused(mut report: StoreReport, code: &str, detail: String) -> StoreReport {
    report.state = StoreState::Refused;
    report.error_code = Some(code.to_owned());
    report.error = Some(detail);
    report
}

fn read_legacy_store(context: &Context) -> Result<Option<Vec<u8>>, String> {
    let data = match TrustedDir::open_absolute(&context.paths().data_dir, DATA_MODE) {
        Ok(data) => data,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(format!("cannot open metadata directory: {error}")),
    };
    let exists = data
        .entry_identity_with_mode(METADATA_STORE_NAME, EntryKind::RegularFile, STORE_MODE)
        .map_err(|error| format!("cannot inspect metadata store: {error}"))?;
    if exists.is_none() {
        return Ok(None);
    }
    data.read_file(
        METADATA_STORE_NAME,
        STORE_MODE,
        settings::ROLLBACK_STORE_BYTES,
    )
    .map(Some)
    .map_err(|error| format!("cannot read metadata store: {error}"))
}
