//! The `refresh-path` transaction.
//!
//! The recorded search path and the daemon job change together or not at all.
//! A fresh run follows the order below; every step is journaled in a
//! [`Operation::RefreshPath`] record before the next one starts.
//!
//! 1. Under the lock: the pending record decides (see [`Engine::refresh_path`]),
//!    the new path is validated, the daemon job must exist, and the adoption
//!    gate judges the live sessions with the new `PATH` before any effect.
//! 2. `config`: `service.toml` is rewritten, in place and atomically.
//! 3. `definition`: the new job definition is verified.
//! 4. The gate judges again right before the swap; a refusal rolls the path
//!    back with no daemon effect.
//! 5. `registering`/`registered`: the job is replaced. The daemon is the only
//!    process restarted; worker jobs keep running.
//! 6. `ready`: the daemon answers as the active version from a new process.
//!
//! The record names the file digests before and after the write, so a resume
//! or a rollback acts only on a file it can account for.

// Rust guideline compliant 2026-10-30

use super::{
    check_layout_dirs, check_rendering_size, config_digest, daemon_definition, file_exists,
    job_alive, job_path, job_path_for, job_warnings, layout, preflight, record, replaces_daemon,
    require_manager_variables, rollback_daemon_dir, rollback_restarts_previous, supervisor,
    supervisor_error, unknown_keys_warning, upgrade_preflight, Engine, Error, Gated, InstallLayout,
    JobDefinition, JobPath, LateGate, Operation, PathBuf, PathUse, Plan, Record, ServiceConfig,
    ServiceState, Step,
};
use crate::service::error::{FileEffect, JobMissingEffects, RecordEffect};
use crate::service::record::{JournaledProcess, RefreshJournal};
use crate::service::refresh::{self, Discovered};
use crate::service::report::{RefreshPathReport, SearchPathReport};
use pohunek_platform::process::ProcessIdentity;
use pohunek_platform::shell_env::SearchPath;
use std::path::Path;

/// What the installed `service.toml` is, relative to a refresh's journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    /// The bytes the refresh started from: nothing was written yet.
    Before,
    /// The bytes the refresh writes.
    After,
    /// The previous search path in its own rendering: a rollback restored it.
    Restored,
    /// Anything else: the file was edited while the refresh was pending.
    Edited,
}

/// What a `refresh-path` run found when it started, to tell afterwards what
/// it changed.
struct RunStart {
    digest: String,
    previous: Vec<PathBuf>,
    target: Option<Vec<PathBuf>>,
    had_record: bool,
}

impl Engine<'_> {
    /// Replaces the recorded search path with `discovered` and the daemon job
    /// with it, or resolves a pending refresh; with `dry_run` it only judges.
    ///
    /// A dry run takes no lock and writes nothing. A pending record is
    /// decided under the lock: a record of another transaction refuses with
    /// [`Error::RefreshBlockedByTransaction`]; a refresh that is rolling back
    /// is finished and the run stops; one before `registering` resumes with
    /// the journaled path (a warning names a different fresh discovery); one
    /// from `registering` on resumes the replacement and its readiness proof;
    /// one at `ready` is only cleared. A resume or rollback never needs the
    /// fresh discovery, so a failed one is carried as a warning; a fresh
    /// refresh returns it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInstalled`] without an installation,
    /// [`Error::RefreshConfigSchema`] for a file of another schema,
    /// [`Error::DaemonJobMissing`] without a daemon job,
    /// [`Error::RefreshConfigChanged`] when `service.toml` was edited during a
    /// pending refresh that has to act on the file, [`Error::RefreshPending`]
    /// for a dry run over any pending refresh (a dry run never resolves one),
    /// the adoption refusals ([`Error::RefreshAtRisk`],
    /// [`Error::RefreshStoreUnusable`], [`Error::RefreshPreflightFailed`])
    /// before any effect, and the failing step's error after a rollback.
    pub(crate) async fn refresh_path(
        &self,
        discovered: Result<Discovered, Error>,
        dry_run: bool,
    ) -> Result<RefreshPathReport, Error> {
        let _transaction = if dry_run {
            None
        } else {
            self.transaction().await?
        };
        upgrade_preflight(self.context)?;
        let config_path = self.context.config_path();
        if !file_exists(&config_path)? {
            return Err(Error::NotInstalled { path: config_path });
        }
        let pending = self.store.load()?;
        if pending
            .as_ref()
            .is_some_and(|record| record.operation != Operation::RefreshPath)
        {
            return Err(Error::RefreshBlockedByTransaction);
        }
        let (config, bytes) = refresh::load_installed_config(self.context)?;
        let start = RunStart {
            digest: config_digest(&bytes),
            previous: pending.as_ref().map_or_else(
                || config.search_path().entries().to_vec(),
                |record| {
                    record
                        .refresh
                        .as_ref()
                        .map_or_else(Vec::new, |j| j.previous.clone())
                },
            ),
            target: pending
                .as_ref()
                .and_then(|record| record.refresh.as_ref().map(|j| j.target.clone())),
            had_record: pending.is_some(),
        };
        let result = match pending {
            Some(record) => {
                self.refresh_pending(record, &config, &bytes, discovered, dry_run)
                    .await
            }
            None => {
                self.refresh_fresh(&config, &bytes, discovered, dry_run)
                    .await
            }
        };
        match result {
            Err(error) if error.is_job_missing() => {
                Err(match self.job_missing_effects(&start, dry_run) {
                    Some(effects) => error.with_job_missing_effects(effects),
                    None => error,
                })
            }
            other => other,
        }
    }

    /// What the run that just ended left in `service.toml` and in the record,
    /// read from the files; `None` when either cannot be read, so the refusal
    /// then claims nothing.
    ///
    /// A dry run never writes, so its effects are stated without a re-read.
    fn job_missing_effects(&self, start: &RunStart, dry_run: bool) -> Option<JobMissingEffects> {
        if dry_run {
            return Some(JobMissingEffects {
                file: FileEffect::Untouched,
                record: if start.had_record {
                    RecordEffect::Kept
                } else {
                    RecordEffect::Absent
                },
            });
        }
        let record = match self.store.load().ok()? {
            None if start.had_record => RecordEffect::Cleared,
            None => RecordEffect::Absent,
            Some(record) if record.rolling_back => RecordEffect::KeptRollingBack,
            Some(_) => RecordEffect::Kept,
        };
        let (config, bytes) = refresh::load_installed_config(self.context).ok()?;
        let entries = config.search_path().entries();
        let file = if config_digest(&bytes) == start.digest {
            FileEffect::Untouched
        } else if entries == start.previous.as_slice() {
            FileEffect::RestoredToPrevious
        } else if start.target.as_deref() == Some(entries) {
            FileEffect::NewPath
        } else {
            return None;
        };
        Some(JobMissingEffects { file, record })
    }

    /// Starts a refresh, or reports that nothing needs one.
    async fn refresh_fresh(
        &self,
        config: &ServiceConfig,
        bytes: &[u8],
        discovered: Result<Discovered, Error>,
        dry_run: bool,
    ) -> Result<RefreshPathReport, Error> {
        let discovered = discovered?;
        let recorded = config.search_path().entries().to_vec();
        let mut report = base_report(
            dry_run,
            recorded.clone(),
            discovered.search_path.entries().to_vec(),
            Some(&discovered.found),
            unknown_keys_warning(config).into_iter().collect(),
        );
        if !report.changed {
            return Ok(report);
        }
        let config_path = self.context.config_path();
        let layout = config.layout().clone();
        check_layout_dirs(self.context, &layout)?;
        layout::verify_claim(&layout, &config.namespace())?;
        let target = config.with_search_path(discovered.search_path.clone())?;
        check_rendering_size(&target, &config_path)?;
        let definition = daemon_definition(self.context, &target)?;
        let observation = match self.backend.daemon().inspect().await {
            Ok(observation) => observation,
            Err(supervisor::Error::NotFound(_)) => {
                return Err(Error::DaemonJobMissing { effects: None })
            }
            Err(source) => return Err(supervisor_error("inspect daemon", source)),
        };
        report.daemon_started = observation.state != ServiceState::Running;
        let job = job_path(self.context, &target, None).await?;
        let warnings = job_warnings(self.context, &job);
        let mut gated = self.gate_refresh(&target, &job, &warnings).await?;
        if dry_run {
            report.daemon_restart = "would_restart";
            report.accepted_runtime_loss = gated.accepted;
            report.warnings.extend(warnings);
            report.warnings.append(&mut gated.warnings);
            report.preflight = Some(gated.report);
            report.workers_kept = self.workers_kept().await;
            return Ok(report);
        }
        let record = Record {
            schema_version: record::SCHEMA_VERSION,
            operation: Operation::RefreshPath,
            version: config.active_version().to_owned(),
            prefix: layout.prefix().to_path_buf(),
            previous_version: Some(config.active_version().to_owned()),
            version_dir_preexisted: true,
            config_backup: None,
            step: Step::Started,
            rolling_back: false,
            runtime_stop: false,
            runtime_backup: None,
            launchd_default_path: None,
            launchd_boot_identity: None,
            accepted_risks: None,
            refresh: Some(RefreshJournal {
                previous: recorded,
                target: discovered.search_path.entries().to_vec(),
                digest_before: config_digest(bytes),
                digest_after: config_digest(target.to_toml().as_bytes()),
                daemon_before: None,
            }),
        };
        self.store.save(&record)?;
        self.checkpoint(Step::Started)?;
        self.execute_refresh(record, &target, &definition, report, false)
            .await
    }

    /// Resolves a refresh an earlier run left pending.
    async fn refresh_pending(
        &self,
        record: Record,
        config: &ServiceConfig,
        bytes: &[u8],
        discovered: Result<Discovered, Error>,
        dry_run: bool,
    ) -> Result<RefreshPathReport, Error> {
        if dry_run {
            return Err(Error::RefreshPending {
                step: record.step.as_str(),
                rolling_back: record.rolling_back,
                restored: None,
            });
        }
        let journal = self.refresh_journal(&record)?.clone();
        let mut warnings: Vec<String> = unknown_keys_warning(config).into_iter().collect();
        let (fresh, found) = match discovered {
            Ok(discovered) => (
                Some(discovered.search_path.entries().to_vec()),
                Some(discovered.found),
            ),
            Err(error) => {
                warnings.push(format!(
                    "the login shell discovery failed ({error}); the pending refresh was resolved without it"
                ));
                (None, None)
            }
        };
        let mut report = base_report(
            false,
            journal.previous.clone(),
            fresh.clone().unwrap_or_else(|| journal.target.clone()),
            found.as_ref(),
            warnings,
        );
        if record.rolling_back {
            let restarts = rollback_restarts_previous(&record);
            let (_resume, rolled_back) = self.resolve_plan(Plan::RollBack(record)).await?;
            report.accepted_runtime_loss = rolled_back
                .as_ref()
                .is_some_and(|rolled_back| rolled_back.accepted_runtime_loss);
            report.rolled_back = rolled_back;
            if restarts {
                report.daemon_restart = "restarted";
                report.workers_kept = self.workers_kept().await;
            }
            report.warnings.push(
                "the pending search-path refresh was rolled back and this run stopped there; run `pohunek service refresh-path` again to record the current path"
                    .to_owned(),
            );
            return Ok(report);
        }
        // The journaled path wins; a fresh discovery that differs is only reported.
        report.discovered.clone_from(&journal.target);
        report.added = journal
            .target
            .iter()
            .filter(|entry| !journal.previous.contains(entry))
            .cloned()
            .collect();
        report.removed = journal
            .previous
            .iter()
            .filter(|entry| !journal.target.contains(entry))
            .cloned()
            .collect();
        report.changed = journal.previous != journal.target;
        if fresh
            .as_deref()
            .is_some_and(|fresh| fresh != journal.target.as_slice())
        {
            report.warnings.push(
                "the login shell reports a search path that differs from the one the interrupted refresh journaled; the journaled one was applied, so rerun `pohunek service refresh-path` to record the current one"
                    .to_owned(),
            );
        }
        report.resumed = true;
        if record.step >= Step::Ready {
            // Only the clear is left. The file was read and verified above but
            // is not classified against the journal, so an edit of it cannot
            // keep a finished refresh pending.
            self.checkpoint(Step::Ready)?;
            self.store.clear()?;
            report.applied = true;
            report.daemon_restart = "restarted";
            report.workers_kept = self.workers_kept().await;
            return Ok(report);
        }
        // The forward path continues from the file as the refresh left it or
        // as it started. A file restored to the previous path's own rendering
        // (by hand: a rollback marks the record `rolling_back` first) holds no
        // refresh write, so it is written again like the unchanged original.
        let target_path = self.journaled_path(&journal.target)?;
        let target = config.with_search_path(target_path)?;
        let state = classify_file(&journal, config, bytes, &target);
        if state == FileState::Edited {
            return Err(Error::RefreshConfigChanged);
        }
        let config_path = self.context.config_path();
        check_layout_dirs(self.context, config.layout())?;
        layout::verify_claim(config.layout(), &config.namespace())?;
        check_rendering_size(&target, &config_path)?;
        let definition = daemon_definition(self.context, &target)?;
        self.execute_refresh(
            record,
            &target,
            &definition,
            report,
            state != FileState::After,
        )
        .await
    }

    /// Runs the remaining steps of `record`, rolling the refresh back when one
    /// fails, and completes `report`.
    ///
    /// `rewrite` writes `target` again even past the `config` step, for a file
    /// that holds the pre-refresh bytes.
    async fn execute_refresh(
        &self,
        mut record: Record,
        target: &ServiceConfig,
        definition: &JobDefinition,
        mut report: RefreshPathReport,
        rewrite: bool,
    ) -> Result<RefreshPathReport, Error> {
        let mut late = LateGate::default();
        let result = self
            .run_refresh_steps(&mut record, target, definition, &mut late, rewrite)
            .await
            // Only a kept record is pending; a refusal before `registering`
            // rolled the refresh back.
            .map_err(|error| match error {
                Error::UntrustedVersion { path, detail } if record.step >= Step::Registering => {
                    Error::UntrustedRefreshVersion {
                        version: record.version.clone(),
                        path,
                        detail,
                    }
                }
                other => other,
            });
        // A refusal never touches the running daemon. Rolling back a record at
        // `Registering` would replace it, and the replacement may not have
        // happened yet, so that record is kept for a resume instead.
        let result = match result {
            Err(error) if late.refused && record.step >= Step::Registering => return Err(error),
            other => other,
        };
        self.settle(&record, result).await?;
        report.applied = true;
        report.daemon_restart = "restarted";
        report.warnings.extend(late.warnings);
        if let Some(mut gated) = late.gated {
            report.accepted_runtime_loss |= gated.accepted;
            report.warnings.append(&mut gated.warnings);
            report.preflight = Some(gated.report);
        }
        report.workers_kept = self.workers_kept().await;
        Ok(report)
    }

    /// The journaled steps of a refresh after the record exists.
    async fn run_refresh_steps(
        &self,
        record: &mut Record,
        target: &ServiceConfig,
        definition: &JobDefinition,
        late: &mut LateGate,
        rewrite: bool,
    ) -> Result<(), Error> {
        if record.step < Step::Config || rewrite {
            target.write(&self.context.config_path())?;
        }
        if record.step < Step::Config {
            self.advance(record, Step::Config)?;
        }
        self.checkpoint(Step::Config)?;

        if record.step < Step::Definition {
            self.prepare_definition(target, definition).await?;
            self.advance(record, Step::Definition)?;
        }
        self.checkpoint(Step::Definition)?;

        if replaces_daemon(record.step) {
            #[cfg(test)]
            if let Some(hook) = &self.before_swap {
                (hook.0)();
            }
            let job = job_path(self.context, target, None).await?;
            let warnings = job_warnings(self.context, &job);
            late.warnings.extend(warnings.iter().cloned());
            let daemon_dir = target
                .daemon_executable()
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| Error::Record {
                    path: self.store.path(),
                    detail: "the daemon executable has no directory".to_owned(),
                })?;
            self.pass_swap_gate(
                record,
                target.layout(),
                &daemon_dir,
                &job,
                &warnings,
                preflight::ReleasePlan::Omitted,
                late,
            )
            .await?;
            // Past `Registering` the replacement may have happened, and the
            // process observed then would be the new one.
            if record.step < Step::Registering {
                self.journal_daemon_before(record).await?;
            }
            self.advance(record, Step::Registering)?;
            self.checkpoint(Step::Registering)?;
            self.register(record.operation, definition, false).await?;
            self.advance(record, Step::Registered)?;
        }
        self.checkpoint(Step::Registered)?;

        if record.step < Step::Ready {
            let replaced = self
                .refresh_journal(record)?
                .daemon_before
                .map(ProcessIdentity::from);
            self.wait_restarted(&record.version, replaced).await?;
            self.advance(record, Step::Ready)?;
        }
        self.checkpoint(Step::Ready)?;
        self.store.clear()
    }

    /// Journals the daemon job's main process right before it is replaced.
    async fn journal_daemon_before(&self, record: &mut Record) -> Result<(), Error> {
        let observation = match self.backend.daemon().inspect().await {
            Ok(observation) => observation,
            Err(supervisor::Error::NotFound(_)) => {
                return Err(Error::DaemonJobMissing { effects: None })
            }
            Err(source) => return Err(supervisor_error("inspect daemon", source)),
        };
        let journal = record.refresh.as_mut().ok_or_else(|| Error::Record {
            path: self.store.path(),
            detail: "a refresh record has no refresh journal".to_owned(),
        })?;
        journal.daemon_before = observation.process.map(JournaledProcess::from);
        self.store.save(record)
    }

    /// Judges the live sessions against the new daemon with the job `PATH`
    /// `job`, as the upgrade's early gate does, and applies the operator's choice.
    ///
    /// The release is the installed one, so the judgement carries no runtime plan.
    async fn gate_refresh(
        &self,
        target: &ServiceConfig,
        job: &JobPath,
        warnings: &[String],
    ) -> Result<Gated, Error> {
        let daemon_dir = target
            .daemon_executable()
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| Error::Record {
                path: self.store.path(),
                detail: "the daemon executable has no directory".to_owned(),
            })?;
        let daemon_dir = layout::verify_installed(target.layout(), &daemon_dir)?;
        let acceptance = self.acceptance(target.active_version())?;
        let gated = preflight::gate_refresh(
            &*self.preflight,
            self.context,
            &daemon_dir,
            self.reported(target.active_version()),
            &job.value,
            &job.variables,
            self.accept_runtime_loss,
            &acceptance,
        )
        .await
        .map_err(|error| error.with_warnings(warnings))?;
        require_manager_variables(job, &gated.report, self.accept_runtime_loss)?;
        Ok(gated)
    }

    /// Undoes a refresh: restores the previous path and, once the job may
    /// have been replaced, replaces it again with the previous definition.
    ///
    /// The record is marked `rolling_back` before the first effect, so a
    /// refused gate leaves a record that only a rerun of `refresh-path` (or the
    /// uninstall) resolves. The file must be one the journal accounts for
    /// before anything changes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefreshConfigChanged`] for a file the journal does not
    /// account for, [`Error::RefreshConfigSchema`] for a file of another
    /// schema, the rollback gate's refusals, [`Error::DaemonJobMissing`] when the
    /// job vanished, and the service-manager and readiness errors of the
    /// restoring replacement.
    pub(super) async fn rollback_refresh(
        &self,
        record: &Record,
        layout: &InstallLayout,
    ) -> Result<Option<Gated>, Error> {
        let journal = self.refresh_journal(record)?;
        // The strict current-schema reader is kept: re-rendering a file of
        // another schema here would migrate it without a migration. A file of
        // another schema is refused with `service_refresh_config_schema`,
        // whose hint names the installed release's own `refresh-path`, and
        // `uninstall` does not need the read.
        let (current, bytes) = refresh::load_installed_config(self.context)?;
        let target = current.with_search_path(self.journaled_path(&journal.target)?)?;
        let state = classify_file(journal, &current, &bytes, &target);
        let previous = self.journaled_path(&journal.previous)?;
        let restored = match state {
            FileState::After => current.with_search_path(previous)?,
            FileState::Before | FileState::Restored => current,
            FileState::Edited => return Err(Error::RefreshConfigChanged),
        };
        self.mark_rolling_back(record)?;
        let restarts = rollback_restarts_previous(record);
        let mut gated = None;
        if restarts {
            let daemon_dir =
                rollback_daemon_dir(layout, &record.version).map_err(|error| match error {
                    Error::UntrustedVersion { path, detail } => Error::UntrustedRefreshVersion {
                        version: record.version.clone(),
                        path,
                        detail,
                    },
                    other => other,
                })?;
            let job = job_path_for(self.context, &restored, None, PathUse::Rollback).await?;
            gated = Some(
                self.judge_rollback(layout, &record.version, &daemon_dir, &job)
                    .await
                    .map_err(|error| match error {
                        Error::RollbackAtRisk {
                            version,
                            sessions,
                            stopped_sessions,
                            warnings,
                        } => Error::RefreshRollbackAtRisk {
                            version,
                            sessions,
                            stopped_sessions,
                            warnings,
                        },
                        other => other,
                    })?,
            );
        }
        if state == FileState::After {
            restored.write(&self.context.config_path())?;
        }
        if restarts {
            let definition = daemon_definition(self.context, &restored)?;
            let replaced = match self.backend.daemon().inspect().await {
                Ok(observation) => observation.process,
                Err(supervisor::Error::NotFound(_)) => None,
                Err(source) => return Err(supervisor_error("inspect daemon", source)),
            };
            // A job that vanished after the record exists cannot be restored by
            // a replacement, and no rerun can change that; the typed error
            // carries the way out.
            self.backend
                .daemon()
                .replace(&definition)
                .await
                .map_err(|source| match source {
                    supervisor::Error::NotFound(_) => Error::DaemonJobMissing { effects: None },
                    source => supervisor_error("restore daemon", source),
                })?;
            self.wait_restarted(&record.version, replaced).await?;
        }
        Ok(gated)
    }

    /// The journal of a refresh record.
    fn refresh_journal<'r>(&self, record: &'r Record) -> Result<&'r RefreshJournal, Error> {
        record.refresh.as_ref().ok_or_else(|| Error::Record {
            path: self.store.path(),
            detail: "a refresh record has no refresh journal".to_owned(),
        })
    }

    /// Validates a journaled search path.
    fn journaled_path(&self, entries: &[PathBuf]) -> Result<SearchPath, Error> {
        SearchPath::new(entries.to_vec()).map_err(|error| Error::Record {
            path: self.store.path(),
            detail: format!("the journaled search path is invalid: {error}"),
        })
    }

    /// The worker jobs the daemon restart leaves running, when they can be listed.
    async fn workers_kept(&self) -> Option<usize> {
        let workers = self.backend.workers().discover().await.ok()?;
        Some(workers.iter().filter(|worker| job_alive(worker)).count())
    }
}

/// Which of the journaled states the installed file is in.
///
/// The rendering of a path restored by a rollback differs from the bytes the
/// refresh started from only in formatting; it counts as restored when the
/// file is its own canonical rendering, records the previous path, and holds
/// every other value the refresh wrote: `target` is the file's configuration
/// with the journaled target path, whose rendering is the one `digest_after`
/// names.
fn classify_file(
    journal: &RefreshJournal,
    config: &ServiceConfig,
    bytes: &[u8],
    target: &ServiceConfig,
) -> FileState {
    let digest = config_digest(bytes);
    if digest == journal.digest_after {
        FileState::After
    } else if digest == journal.digest_before {
        FileState::Before
    } else if config.search_path().entries() == journal.previous.as_slice()
        && bytes == config.to_toml().as_bytes()
        && config_digest(target.to_toml().as_bytes()) == journal.digest_after
    {
        FileState::Restored
    } else {
        FileState::Edited
    }
}

/// The report of a run that has not decided anything yet; `found` is `None`
/// when no discovery ran or it failed.
fn base_report(
    dry_run: bool,
    recorded: Vec<PathBuf>,
    discovered: Vec<PathBuf>,
    found: Option<&SearchPathReport>,
    warnings: Vec<String>,
) -> RefreshPathReport {
    let added = discovered
        .iter()
        .filter(|entry| !recorded.contains(entry))
        .cloned()
        .collect();
    let removed = recorded
        .iter()
        .filter(|entry| !discovered.contains(entry))
        .cloned()
        .collect();
    RefreshPathReport {
        dry_run,
        changed: recorded != discovered,
        applied: false,
        recorded,
        discovered,
        added,
        removed,
        shell_used: found.and_then(|found| found.shell_used.clone()),
        shell_defaulted: found.is_some_and(|found| found.shell_defaulted),
        dropped: found.map(|found| found.dropped.clone()).unwrap_or_default(),
        daemon_restart: "none",
        daemon_started: false,
        workers_kept: None,
        resumed: false,
        rolled_back: None,
        preflight: None,
        accepted_runtime_loss: false,
        warnings,
    }
}
