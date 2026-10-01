//! The install, upgrade, uninstall, and status transactions.
//!
//! # Install and upgrade
//!
//! Binaries are first staged privately and their `--version` verified. The
//! transaction record is then written, and each step below is journaled in it
//! before the next one starts:
//!
//! 1. `binaries`: the staged directory is renamed to
//!    `<prefix>/libexec/pohunek/<version>/` (an identical existing directory
//!    is reused, a different one is refused);
//! 2. `config`: `service.toml` names the version;
//! 3. `definition`: the daemon job definition is built and, on Linux,
//!    verified with `systemd-analyze verify`;
//! 4. `registering`/`registered`: the service manager installs (install) or
//!    replaces (upgrade) the daemon job, restarting only the daemon;
//! 5. `ready`: the daemon answers `daemon.health` as the new version, on a
//!    connection whose kernel peer credentials name the process the service
//!    manager reports as the running daemon job's main process;
//! 6. `cli`: `<prefix>/bin/pohunek` becomes the new CLI.
//!
//! Before any effect, including the rollback of a pending record, install,
//! upgrade, and uninstall refuse a prefix `service.toml` would reject and an
//! XDG root or `HOME` that is not UTF-8, which the daemon's job definition
//! cannot carry. Install and upgrade also refuse an untrusted directory they
//! would write into and a `HOME` the daemon cannot start workers in. A command
//! that cannot finish therefore never undoes another transaction first.
//! `pohunek service check` runs these checks alone through the same functions
//! (`install_preflight`, `upgrade_preflight`, `check_layout_dirs`,
//! `install_plan`, `upgrade_plan`).
//!
//! Before staging anything, install and upgrade claim the prefix for this
//! installation's namespace (see [`super::layout`]): a prefix another
//! namespace owns is refused with `service_prefix_owned`, because version
//! collection and uninstall see only their own namespace's references.
//! Rollback, collection, and uninstall verify the claim before deleting
//! anything; a rolled-back install and a finished uninstall release it.
//!
//! A failing upgrade step rolls the transaction back: the daemon job is
//! replaced by the previous version, `service.toml` is restored, and a
//! version directory this transaction created is removed unless something
//! references it. An install that fails before `registering` rolls back the
//! same way, removing `service.toml` instead. A failure after `ready` keeps
//! the record instead, because the service already runs the new version; the
//! next run finishes the CLI step.
//!
//! An install that fails at or after `registering` also keeps its record, the
//! daemon job, and `service.toml`, and fails with
//! `service_install_incomplete`. The registration call can fail after the
//! service manager really registered the job (a D-Bus or `launchctl`
//! timeout), and that daemon may already own live sessions, so only the
//! resume or the session-checked uninstall below may decide its fate.
//!
//! A rollback marks the record `rolling_back` before its first effect. It
//! restores `service.toml` before the daemon, so a rollback that fails or is
//! interrupted partway leaves a record whose later steps are already undone;
//! a later command never resumes it, and finishes that rollback before it
//! starts its own transaction.
//!
//! An interrupted process leaves the record behind. The next `install` or
//! `upgrade` of the same version and prefix resumes after the recorded step
//! unless the record is rolling back.
//! Any other operation rolls the record back first, except that a
//! transaction which may already have registered its daemon (`registering`
//! or later) is never rolled back: `upgrade` and a different `install`
//! refuse it with `service_install_pending`, and `uninstall` removes it
//! through the full uninstall below, whose session inventory and
//! `--stop-sessions` gating are the only safe way to stop a daemon that may
//! own live workers. Every step is idempotent, so resuming never duplicates
//! a job or a directory.
//!
//! After a successful upgrade, version directories that are neither active,
//! referenced by the journal of a worker that may still run, named by a
//! registered worker job, executed by a running process, nor holding the
//! running CLI are removed. When worker jobs cannot be discovered, every
//! version is kept. An upgrade to the already active version restarts
//! nothing but runs the same collection, so a version kept for a reason that
//! has since ended is removed by rerunning the upgrade.
//!
//! # Concurrency
//!
//! Install, upgrade, and uninstall each hold an exclusive `flock` on
//! `<state>/pohunek/service-install.lock` for their whole run, covering
//! resume, rollback of a pending record, and garbage collection. A second
//! command waits at most two seconds (enough to ride out a `status` probe)
//! and then fails with `service_transaction_in_progress`; it never touches
//! a record whose writer is alive. A crashed holder's lock is released by the
//! kernel, so its record is resumed or rolled back by the next command.
//! A command started under `pohunek service lock` instead adopts the lock its
//! ancestor holds ([`Engine::with_inherited_lock`]); it never takes a second
//! one, which that ancestor's own lock would refuse.
//!
//! # Uninstall
//!
//! The daemon must enumerate sessions, so an installed but stopped daemon is
//! started through the service manager first. Live sessions refuse the
//! uninstall unless `--stop-sessions` is given; then each is stopped through
//! the daemon and the engine waits until every worker's journal is final and
//! its process is gone. Only then are the daemon job, the remaining ended
//! worker jobs, the launchd directories, the installed `<prefix>/bin/pohunek`
//! copy, unreferenced version directories, and the prefix ownership record
//! removed; the record stays while a version is kept for a worker journal or
//! job, which only this namespace can see. `--purge` also removes the session
//! store, event logs, worker journals, and host identity. The transaction
//! record is cleared next and `service.toml` goes last: it names the prefix,
//! so while any earlier removal fails, or the process dies before the end, a
//! rerun of `uninstall` still finds the installation and finishes the
//! cleanup.
//!
//! A pending install that already reached the registration step takes this
//! same flow instead of a rollback: its daemon may already run it with live
//! workers, and only the session-checked removal above may stop them. Such a
//! record found without `service.toml` describes its installation through
//! its version and prefix, and the uninstall runs the same flow with the
//! configuration the install wrote.
//!
//! Worker journals an older pohunek wrote under an earlier schema block the
//! uninstall while their worker may still run. The daemon of this version
//! cannot stop those workers, so the uninstall names them instead of
//! waiting on them.

// Rust guideline compliant 2026-09-29

use std::path::Path;
use std::time::{Duration, Instant};

use pohunek_paths::InstallLayout;
use pohunek_platform::process::{HostInspector, Pid, ProcessInspector};
use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::{
    self, JobDefinition, Namespace, ServiceObservation, ServiceState, WorkerKey,
};
use pohunek_service_config::ServiceConfig;
use protocol::{RuntimeState, SessionInfo, SessionState};

use super::backend::Backend;
use super::context::Context;
use super::definition::{
    config_with_search_path, daemon_definition, identity_config, with_version,
};
use super::error::{supervisor_error, Error, LiveSession, OutdatedWorker};
use super::layout::{self, Staged};
use super::record::{self, Operation, Record, Step, Store, TransactionLock};
use super::report::{
    state_name, InstallReport, JobReport, KeptVersion, PendingReport, SearchPathReport,
    StatusReport, UninstallReport, UpgradeReport, VersionReport, WorkerReport,
};
use super::settings;
use super::usage::{Journals, Usage};

/// Durable metadata `--purge` removes from the data directory.
///
/// Names match the daemon's store (`metadata.jsonl`) and event log
/// directory (`events`). Worktrees, notifications, and configuration are
/// never purged: they may hold the owner's work.
const PURGED_DATA_FILES: [&str; 1] = ["metadata.jsonl"];

/// Durable metadata directories `--purge` removes from the data directory.
const PURGED_DATA_DIRS: [&str; 1] = ["events"];

/// Durable metadata directories `--purge` removes from the state directory:
/// worker journals and the host identity.
const PURGED_STATE_DIRS: [&str; 2] = [
    pohunek_paths::WORKERS_SUBDIR,
    pohunek_paths::HOST_STATE_SUBDIR,
];

/// Options of `pohunek service uninstall`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UninstallOptions {
    /// Stop live sessions instead of refusing.
    pub stop_sessions: bool,
    /// Also remove durable metadata.
    pub purge: bool,
}

/// Runs service transactions against one backend.
#[derive(Debug)]
pub struct Engine<'a> {
    context: &'a Context,
    backend: &'a Backend,
    /// Observes worker and version-referencing processes; the host's process
    /// table in production.
    inspector: Box<dyn ProcessInspector>,
    store: Store,
    /// Transaction lock an ancestor `pohunek service lock` handed down; every
    /// transaction of this engine runs under it instead of a lock of its own.
    inherited: Option<TransactionLock>,
    ready_timeout: Duration,
    stop_timeout: Duration,
    #[cfg(test)]
    interrupt_after: Option<Step>,
    #[cfg(feature = "test-util")]
    reported_version: Option<String>,
}

impl<'a> Engine<'a> {
    /// Creates an engine for `context` using `backend`.
    #[must_use]
    pub fn new(context: &'a Context, backend: &'a Backend) -> Self {
        Self {
            context,
            backend,
            inspector: Box::new(HostInspector::new()),
            store: Store::new(context.paths().state_dir.clone()),
            inherited: None,
            ready_timeout: settings::DAEMON_READY_TIMEOUT,
            stop_timeout: settings::SESSION_STOP_TIMEOUT,
            #[cfg(test)]
            interrupt_after: None,
            #[cfg(feature = "test-util")]
            reported_version: None,
        }
    }

    /// Runs every transaction under `lock`, which an ancestor process holds.
    ///
    /// `lock` comes from [`Store::adopt`]; the engine then never takes the
    /// transaction lock itself.
    #[must_use]
    pub fn with_inherited_lock(mut self, lock: TransactionLock) -> Self {
        self.inherited = Some(lock);
        self
    }

    /// Takes the transaction lock unless an inherited one covers this engine.
    ///
    /// The returned guard, when there is one, must live for the whole
    /// transaction.
    async fn transaction(&self) -> Result<Option<TransactionLock>, Error> {
        if self.inherited.is_some() {
            Ok(None)
        } else {
            self.store.lock().await.map(Some)
        }
    }

    /// Accepts staged binaries, and the daemon they run, reporting
    /// `reported` while installing them under the version directory the
    /// caller names.
    ///
    /// Test-only: one build has one version, so an upgrade between two
    /// version directories with live workers is exercised by installing the
    /// same build twice under different directory names. Everything else
    /// (staging, journal, `service.toml`, daemon replacement, and version GC)
    /// runs unchanged. The previous daemon reports the same version, so the
    /// caller must observe the daemon's restart itself.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn with_reported_version(mut self, reported: impl Into<String>) -> Self {
        self.reported_version = Some(reported.into());
        self
    }

    /// Version the staged binaries must report when installing `version`.
    #[cfg_attr(
        not(feature = "test-util"),
        expect(
            clippy::unused_self,
            reason = "the receiver carries the test-only reported-version override"
        )
    )]
    fn reported<'v>(&'v self, version: &'v str) -> &'v str {
        #[cfg(feature = "test-util")]
        if let Some(reported) = &self.reported_version {
            return reported;
        }
        version
    }

    /// Installs `version` from the binaries in `from` below `prefix`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingEnv`], [`Error::NonUtf8Env`], or
    /// [`Error::UnusableEnv`] before any effect for a `HOME` or bootstrap
    /// root the daemon would refuse at startup,
    /// [`Error::AlreadyInstalled`] when `service.toml` exists,
    /// [`Error::DaemonJobPresent`] for a stale daemon job,
    /// [`Error::PendingInstall`] for another pending install that may already
    /// run its daemon, staging and probe errors, the failing step's error after
    /// a rollback, and [`Error::InstallIncomplete`] when a step at or after
    /// `registering` fails, keeping the record for a resume or an uninstall.
    pub async fn install(
        &self,
        from: &Path,
        prefix: &Path,
        version: &str,
    ) -> Result<InstallReport, Error> {
        let _transaction = self.transaction().await?;
        let layout = install_preflight(self.context, prefix)?;
        let plan = install_plan(self.store.load()?, prefix, version)?;
        // The discovery environment is judged, and discovery run, before a
        // foreign pending transaction is rolled back, so a hostile environment
        // fails the install with nothing changed.
        // An existing installation or job wins over discovery for a fresh plan,
        // so re-running install neither starts the login shell nor masks
        // `service_already_installed`.
        let config_path = self.context.config_path();
        if matches!(plan, Plan::Fresh) {
            self.ensure_not_installed(&config_path).await?;
        }
        let discovered = discover_for_plan(self.context, &plan)?;
        let (resume, rolled_back) = match plan {
            Plan::Fresh => (None, None),
            Plan::Resume(record) => (Some(record), None),
            Plan::RollBack(record) => {
                let report = pending_report(&record);
                self.rollback_pending(&record).await?;
                (None, Some(report))
            }
        };
        // The rollback removed the foreign transaction's files; what is left
        // must not be an installation.
        if rolled_back.is_some() {
            self.ensure_not_installed(&config_path).await?;
        }
        // Once `service.toml` is written it is the installation's record, so a
        // resume past that step reads it instead of probing the host again,
        // after proving it still describes this installation.
        let (config, search_path_report) = match &resume {
            Some(record) if record.step >= Step::Config => {
                let config = verify_resume_config(self.context, prefix, version)?;
                let report = SearchPathReport {
                    source: "recorded",
                    entries: config.search_path().entries().to_vec(),
                    shell_used: None,
                    shell_defaulted: false,
                    login_shell_failure: None,
                    dropped: Vec::new(),
                };
                (config, report)
            }
            _ => {
                let (search_path, report) =
                    discovered.expect("a plan that writes service.toml discovered the search path");
                (
                    config_with_search_path(self.context, prefix, version, search_path)?,
                    report,
                )
            }
        };
        let namespace = config.namespace();
        let claimed = layout::claim_prefix(&layout, &namespace)?;
        let resumed = resume.is_some();
        let started = async {
            let staged = layout::stage(&layout, from, self.reported(version)).await?;
            let record = if let Some(record) = resume {
                record
            } else {
                let record = Record {
                    schema_version: record::SCHEMA_VERSION,
                    operation: Operation::Install,
                    version: version.to_owned(),
                    prefix: prefix.to_path_buf(),
                    previous_version: None,
                    version_dir_preexisted: layout::version_exists(&layout, version)?,
                    step: Step::Started,
                    rolling_back: false,
                };
                self.store.save(&record)?;
                record
            };
            Ok::<_, Error>((staged, record))
        }
        .await;
        let (staged, mut record) = match started {
            Ok(started) => started,
            // Without a record no rollback runs, so a claim this call made
            // is given up here instead.
            Err(error) if claimed => {
                return Err(match layout::release_prefix(&layout, &namespace) {
                    Ok(_released) => error,
                    Err(release) => Error::RollbackFailed {
                        original: Box::new(error),
                        rollback: Box::new(release),
                    },
                });
            }
            Err(error) => return Err(error),
        };
        let result = self.run_steps(&mut record, &layout, staged, &config).await;
        self.settle(&record, result).await?;
        Ok(InstallReport {
            version: version.to_owned(),
            prefix: prefix.to_path_buf(),
            namespace: config.namespace().as_str().to_owned(),
            config_path,
            daemon_executable: config.daemon_executable(),
            resumed,
            rolled_back,
            search_path: search_path_report,
        })
    }

    /// Fails when `service.toml` or a daemon job of this namespace exists.
    async fn ensure_not_installed(&self, config_path: &Path) -> Result<(), Error> {
        if file_exists(config_path)? {
            return Err(Error::AlreadyInstalled {
                path: config_path.to_path_buf(),
            });
        }
        ensure_no_daemon_job(self.backend).await
    }

    /// Upgrades the installation to `version` from the binaries in `from`.
    ///
    /// # Errors
    ///
    /// Returns the environment errors [`Self::install`] returns before any
    /// effect, [`Error::PendingInstall`] while an interrupted install's record
    /// exists, [`Error::NotInstalled`] without `service.toml`, staging and
    /// probe errors, and the failing step's error after a rollback.
    pub async fn upgrade(&self, from: &Path, version: &str) -> Result<UpgradeReport, Error> {
        let _transaction = self.transaction().await?;
        upgrade_preflight(self.context)?;
        let config_path = self.context.config_path();
        let (resume, rolled_back) = match upgrade_plan(self.store.load()?, version)? {
            Plan::Fresh => (None, None),
            Plan::Resume(record) => (Some(record), None),
            Plan::RollBack(record) => {
                let report = pending_report(&record);
                self.rollback_pending(&record).await?;
                (None, Some(report))
            }
        };
        let config = load_config(&config_path)?.ok_or(Error::NotInstalled {
            path: config_path.clone(),
        })?;
        let layout = config.layout().clone();
        check_layout_dirs(self.context, &layout)?;
        layout::claim_prefix(&layout, &config.namespace())?;
        let staged = layout::stage(&layout, from, self.reported(version)).await?;
        let resumed = resume.is_some();
        let from_version = resume
            .as_ref()
            .and_then(|record| record.previous_version.clone())
            .unwrap_or_else(|| config.active_version().to_owned());
        if resume.is_none() && config.active_version() == version {
            layout::verify_existing(&layout, &staged, version)?;
            layout::discard(&layout, &staged)?;
            layout::install_cli(&layout, version)?;
            let (removed_versions, kept_versions, gc_error) =
                self.collect_garbage_reported(&layout, version).await;
            return Ok(UpgradeReport {
                from_version,
                to_version: version.to_owned(),
                unchanged: true,
                resumed: false,
                rolled_back,
                removed_versions,
                kept_versions,
                gc_error,
            });
        }
        let mut record = if let Some(record) = resume {
            record
        } else {
            {
                let record = Record {
                    schema_version: record::SCHEMA_VERSION,
                    operation: Operation::Upgrade,
                    version: version.to_owned(),
                    prefix: layout.prefix().to_path_buf(),
                    previous_version: Some(from_version.clone()),
                    version_dir_preexisted: layout::version_exists(&layout, version)?,
                    step: Step::Started,
                    rolling_back: false,
                };
                self.store.save(&record)?;
                record
            }
        };
        let target = with_version(&config, version)?;
        let result = self.run_steps(&mut record, &layout, staged, &target).await;
        self.settle(&record, result).await?;
        let (removed_versions, kept_versions, gc_error) =
            self.collect_garbage_reported(&layout, version).await;
        Ok(UpgradeReport {
            from_version,
            to_version: version.to_owned(),
            unchanged: false,
            resumed,
            rolled_back,
            removed_versions,
            kept_versions,
            gc_error,
        })
    }

    /// Uninstalls the service following `options`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInstalled`] when nothing is installed, even with
    /// `--purge`; [`Error::DaemonJobPresent`] when `--purge` follows the
    /// rollback of an unregistered install while a daemon job exists;
    /// [`Error::LiveSessions`] when sessions are live without
    /// `--stop-sessions`, [`Error::StopTimeout`] when they do not settle, and
    /// [`Error::OrphanWorkers`] when worker jobs survive retirement.
    pub async fn uninstall(&self, options: UninstallOptions) -> Result<UninstallReport, Error> {
        let _transaction = self.transaction().await?;
        self.context.bootstrap_environment()?;
        let mut report = UninstallReport::default();
        let mut registered = None;
        if let Some(pending) = self.store.load()? {
            // A transaction that may already have registered its daemon
            // cannot be rolled back here: the daemon may run it with live
            // workers, and removing both is the session-checked decision of
            // the uninstall below, which also consumes the record together
            // with the installation.
            if pending.operation == Operation::Install && pending.step >= Step::Registering {
                registered = Some(pending);
            } else if options.purge && pending.operation == Operation::Install {
                // The rollback removes `service.toml`, so only the record lets
                // a rerun after a failed purge find this installation; it is
                // cleared once the purge below finished.
                report.rolled_back = Some(pending_report(&pending));
                self.rollback(&pending).await?;
            } else {
                report.rolled_back = Some(pending_report(&pending));
                self.rollback_pending(&pending).await?;
            }
        }
        let config_path = self.context.config_path();
        let config = match (load_config(&config_path)?, &registered) {
            (Some(config), _) => config,
            // The install wrote `service.toml` before registering, so a
            // missing file was removed afterwards; the record still names the
            // installation, and its daemon may still run live workers.
            (None, Some(pending)) => {
                identity_config(self.context, &pending.prefix, &pending.version)?
            }
            // The rolled back install never registered its daemon, so nothing
            // of it remains; `--purge` still removes the durable metadata.
            (None, None) if report.rolled_back.is_some() => {
                if options.purge {
                    self.purge_unregistered(&mut report).await?;
                }
                self.store.clear()?;
                return Ok(report);
            }
            // Without an installation or its record nothing proves which
            // daemon owns the durable metadata, so `--purge` removes none.
            (None, None) => return Err(Error::NotInstalled { path: config_path }),
        };
        let layout = config.layout().clone();
        // Another namespace's prefix holds nothing of this installation, and
        // its versions and CLI copy must survive this uninstall.
        layout::claim_existing_prefix(&layout, &config.namespace())?;
        let definition = daemon_definition(self.context, &config)?;

        let reachable = self
            .ensure_daemon(&definition, config.active_version(), &mut report)
            .await;
        let sessions = match &reachable {
            Ok(()) => self
                .backend
                .control()
                .sessions()
                .await
                .map_err(|source| Error::Control {
                    operation: "session.list",
                    source,
                })?,
            Err(_unreachable) => Vec::new(),
        };
        let (live, workers) = self.blocking(&sessions).await?;
        if !live.is_empty() || !workers.is_empty() {
            // Live runtimes can be stopped only through the daemon.
            reachable?;
            if !options.stop_sessions {
                return Err(Error::LiveSessions {
                    sessions: live,
                    workers,
                });
            }
            for session in &live {
                // A session that ended on its own meanwhile rejects the stop;
                // settlement below is the authority either way.
                if self.backend.control().stop(&session.id).await.is_ok() {
                    report.stopped_sessions.push(session.id.clone());
                }
            }
            self.wait_settled().await?;
        }

        self.backend
            .daemon()
            .uninstall()
            .await
            .map_err(|source| supervisor_error("uninstall daemon", source))?;
        report.retired_workers = self.retire_ended_workers().await?;

        self.remove_installation(&layout, &config.namespace(), &mut report)
            .await?;
        if options.purge {
            self.purge(&mut report)?;
            report.purged = true;
        }
        // The record goes before `service.toml`: a record left without the
        // file would still route a rerun here, but `service.toml` left
        // without the record is an ordinary installation to finish removing.
        self.store.clear()?;
        if remove_config(&config_path)? {
            report.removed.push(config_path);
        }
        Ok(report)
    }

    /// Reports the installation's state.
    ///
    /// # Errors
    ///
    /// Returns filesystem errors for unsafe directories; service-manager
    /// failures are reported inside the result instead.
    pub async fn status(&self, config: Option<&ServiceConfig>) -> Result<StatusReport, Error> {
        let transaction_in_progress = self.store.in_progress()?;
        let pending_transaction = self.store.load()?.as_ref().map(pending_report);
        let mut report = StatusReport {
            installed: config.is_some(),
            config_path: self.context.config_path(),
            namespace: None,
            prefix: None,
            active_version: None,
            daemon: None,
            daemon_error: None,
            versions: Vec::new(),
            workers: Vec::new(),
            workers_error: None,
            unreadable_journals: Vec::new(),
            pending_transaction,
            transaction_in_progress,
        };
        let Some(config) = config else {
            return Ok(report);
        };
        let layout = config.layout();
        report.namespace = Some(config.namespace().as_str().to_owned());
        report.prefix = Some(layout.prefix().to_path_buf());
        report.active_version = Some(config.active_version().to_owned());
        match self.backend.daemon().inspect().await {
            Ok(observation) => report.daemon = Some(JobReport::from(&observation)),
            Err(supervisor::Error::NotFound(_)) => {}
            Err(error) => report.daemon_error = Some(error.to_string()),
        }
        let discovered = self.backend.workers().discover().await;
        match &discovered {
            Ok(observations) => {
                report.workers = observations
                    .iter()
                    .filter_map(|observation| {
                        let key = WorkerKey::from_service_id(&observation.id).ok()?;
                        let job = JobReport::from(observation);
                        let version = job
                            .executable
                            .as_deref()
                            .and_then(|executable| layout::version_of(layout, executable));
                        Some(WorkerReport {
                            job,
                            session_id: key.session_id().to_owned(),
                            generation: key.generation().to_owned(),
                            version,
                        })
                    })
                    .collect();
            }
            Err(error) => report.workers_error = Some(error.to_string()),
        }
        let journals = Journals::scan(self.context.paths())?;
        report.unreadable_journals.clone_from(&journals.unreadable);
        let usage = self.collect_usage(
            layout,
            &journals,
            discovered.as_deref().map_err(ToString::to_string),
        );
        report.versions = layout::installed_versions(layout)?
            .into_iter()
            .map(|version| VersionReport {
                active: version == config.active_version(),
                journals: usage.journals.get(&version).cloned().unwrap_or_default(),
                pids: usage.processes.get(&version).cloned().unwrap_or_default(),
                version,
            })
            .collect();
        Ok(report)
    }

    /// Runs the remaining steps of an install or upgrade.
    async fn run_steps(
        &self,
        record: &mut Record,
        layout: &InstallLayout,
        staged: Staged,
        config: &ServiceConfig,
    ) -> Result<(), Error> {
        let version = record.version.clone();
        if record.step < Step::Binaries {
            layout::publish(layout, &staged, &version)?;
            self.advance(record, Step::Binaries)?;
        } else {
            layout::verify_existing(layout, &staged, &version)?;
            layout::discard(layout, &staged)?;
        }
        self.checkpoint(Step::Binaries)?;

        if record.step < Step::Config {
            config.write(&self.context.config_path())?;
            self.advance(record, Step::Config)?;
        }
        self.checkpoint(Step::Config)?;

        let definition = daemon_definition(self.context, config)?;
        if record.step < Step::Definition {
            self.prepare_definition(config, &definition).await?;
            self.advance(record, Step::Definition)?;
        }
        self.checkpoint(Step::Definition)?;

        if record.step < Step::Registered {
            let resuming = record.step == Step::Registering;
            self.advance(record, Step::Registering)?;
            self.checkpoint(Step::Registering)?;
            self.register(record.operation, &definition, resuming)
                .await?;
            self.advance(record, Step::Registered)?;
        }
        self.checkpoint(Step::Registered)?;

        if record.step < Step::Ready {
            self.wait_ready(&version).await?;
            self.advance(record, Step::Ready)?;
        }
        self.checkpoint(Step::Ready)?;

        if record.step < Step::Cli {
            layout::install_cli(layout, &version)?;
            self.advance(record, Step::Cli)?;
        }
        self.checkpoint(Step::Cli)?;
        self.store.clear()
    }

    /// Rolls back a failed transaction or keeps its record for resumption.
    async fn settle(&self, record: &Record, result: Result<(), Error>) -> Result<(), Error> {
        let Err(error) = result else {
            return Ok(());
        };
        #[cfg(test)]
        if matches!(error, Error::Interrupted(_)) {
            return Err(error);
        }
        if record.operation == Operation::Install && record.step >= Step::Registering {
            // An ambiguous registration may have left a running daemon, and
            // only the session-checked uninstall may stop one.
            return Err(Error::InstallIncomplete {
                original: Box::new(error),
                version: record.version.clone(),
                prefix: record.prefix.clone(),
                step: record.step.as_str(),
            });
        }
        if record.step >= Step::Ready {
            return Err(error);
        }
        match self.rollback(record).await {
            Ok(()) => {
                self.store.clear()?;
                Err(error)
            }
            Err(rollback) => Err(Error::RollbackFailed {
                original: Box::new(error),
                rollback: Box::new(rollback),
            }),
        }
    }

    /// Rolls back an interrupted transaction found at startup.
    async fn rollback_pending(&self, record: &Record) -> Result<(), Error> {
        self.rollback(record).await?;
        self.store.clear()
    }

    /// Undoes every effect `record`'s transaction may have had.
    ///
    /// The record is marked `rolling_back` before the first effect, so a
    /// rollback that fails or dies partway is finished by the next command
    /// instead of resumed. Every effect is idempotent, which makes a rerun on
    /// a partly rolled back installation safe.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Record`] for an install record at or after
    /// `registering`. Such a transaction may have registered a daemon that
    /// owns live sessions, and removing it here would bypass the uninstall's
    /// session check; `settle`, `install`, `upgrade`, and `uninstall` all
    /// route those records elsewhere, so reaching this is a broken invariant
    /// that must fail loudly rather than stop sessions.
    async fn rollback(&self, record: &Record) -> Result<(), Error> {
        let layout = install_layout(&record.prefix)?;
        let config_path = self.context.config_path();
        match record.operation {
            Operation::Install => {
                if record.step >= Step::Registering {
                    return Err(Error::Record {
                        path: self.store.path(),
                        detail: format!(
                            "an install that reached step {} is never rolled back; \
                             finish it with `pohunek service install` or remove it with \
                             `pohunek service uninstall`",
                            record.step.as_str()
                        ),
                    });
                }
                self.mark_rolling_back(record)?;
                // Install refuses to start while service.toml exists, so any
                // file present now was written by this transaction.
                remove_config(&config_path)?;
            }
            Operation::Upgrade => {
                let previous = record
                    .previous_version
                    .as_deref()
                    .ok_or_else(|| Error::Record {
                        path: self.store.path(),
                        detail: "an upgrade record names no previous version".to_owned(),
                    })?;
                self.mark_rolling_back(record)?;
                let current = load_config(&config_path)?.ok_or(Error::NotInstalled {
                    path: config_path.clone(),
                })?;
                let restored = with_version(&current, previous)?;
                restored.write(&config_path)?;
                if record.step >= Step::Registering {
                    let definition = daemon_definition(self.context, &restored)?;
                    self.backend
                        .daemon()
                        .replace(&definition)
                        .await
                        .map_err(|source| supervisor_error("restore daemon", source))?;
                    self.wait_ready(previous).await?;
                }
            }
        }
        let namespace = self.context.namespace()?;
        layout::claim_existing_prefix(&layout, &namespace)?;
        let install = record.operation == Operation::Install;
        if record.version_dir_preexisted && !install {
            return Ok(());
        }
        let usage = self.usage(&layout).await?;
        if !record.version_dir_preexisted && usage.keep_reason(&record.version, None).is_none() {
            layout::remove_version(&layout, &record.version)?;
        }
        // An install starts only without `service.toml` and is rolled back
        // only before registering, so without a namespace-bound reference
        // this namespace keeps nothing in the prefix.
        if install
            && !layout::installed_versions(&layout)?
                .iter()
                .any(|version| usage.namespace_bound(version))
        {
            layout::release_prefix(&layout, &namespace)?;
        }
        Ok(())
    }

    /// Journals that `record`'s rollback has begun.
    fn mark_rolling_back(&self, record: &Record) -> Result<(), Error> {
        if record.rolling_back {
            return Ok(());
        }
        self.store.save(&Record {
            rolling_back: true,
            ..record.clone()
        })
    }

    fn advance(&self, record: &mut Record, step: Step) -> Result<(), Error> {
        record.step = step;
        self.store.save(record)
    }

    /// Returns the injected test interruption for `step`, if any.
    #[cfg(test)]
    fn checkpoint(&self, step: Step) -> Result<(), Error> {
        if self.interrupt_after == Some(step) {
            Err(Error::Interrupted(step))
        } else {
            Ok(())
        }
    }

    /// Marks the end of a journaled step; production builds never interrupt.
    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "test builds inject interruptions through the same signature"
    )]
    fn checkpoint(&self, _step: Step) -> Result<(), Error> {
        Ok(())
    }

    /// Verifies the rendered units with `systemd-analyze` before registering them.
    #[cfg(target_os = "linux")]
    async fn prepare_definition(
        &self,
        config: &ServiceConfig,
        definition: &JobDefinition,
    ) -> Result<(), Error> {
        let _ = self;
        super::verify::verify_units(&config.namespace(), definition).await
    }

    /// Creates the launchd log directory the daemon agent writes into.
    ///
    /// launchd needs no verification step, so the result is ready at once.
    #[cfg(target_os = "macos")]
    fn prepare_definition(
        &self,
        _config: &ServiceConfig,
        _definition: &JobDefinition,
    ) -> std::future::Ready<Result<(), Error>> {
        std::future::ready(layout_log_dir(self.context))
    }

    async fn register(
        &self,
        operation: Operation,
        definition: &JobDefinition,
        resuming: bool,
    ) -> Result<(), Error> {
        let daemon = self.backend.daemon();
        match (operation, resuming) {
            (Operation::Install, false) => daemon
                .install(definition)
                .await
                .map_err(|source| supervisor_error("install daemon", source)),
            (Operation::Install, true) => match daemon.replace(definition).await {
                Err(supervisor::Error::NotFound(_)) => daemon
                    .install(definition)
                    .await
                    .map_err(|source| supervisor_error("install daemon", source)),
                result => result.map_err(|source| supervisor_error("replace daemon", source)),
            },
            (Operation::Upgrade, _) => daemon
                .replace(definition)
                .await
                .map_err(|source| supervisor_error("replace daemon", source)),
        }
    }

    /// Waits until the supervised daemon answers `daemon.health` as `version`.
    ///
    /// The answer counts only when the process serving the socket is the
    /// daemon job's running main process. Another process of the same build,
    /// such as a manually started daemon, can hold the socket while the
    /// supervised job crash-loops, and must never make a transaction ready.
    async fn wait_ready(&self, version: &str) -> Result<(), Error> {
        let deadline = Instant::now() + self.ready_timeout;
        loop {
            let mut last = match self.backend.control().health().await {
                Ok(health) if health.result.daemon_version == self.reported(version) => {
                    match self.served_by_job(health.pid).await {
                        Ok(()) => return Ok(()),
                        Err(mismatch) => Some(mismatch),
                    }
                }
                Ok(health) => Some(format!(
                    "daemon reports version {}",
                    health.result.daemon_version
                )),
                Err(error) => Some(error.to_string()),
            };
            if Instant::now() >= deadline {
                if let Ok(observation) = self.backend.daemon().inspect().await {
                    let job = JobReport::from(&observation);
                    last = Some(format!(
                        "{}; daemon job {} is {}",
                        last.unwrap_or_default(),
                        job.service_id,
                        job.state
                    ));
                }
                return Err(Error::DaemonNotReady {
                    version: version.to_owned(),
                    timeout: self.ready_timeout,
                    last,
                });
            }
            tokio::time::sleep(settings::POLL_INTERVAL).await;
        }
    }

    /// Checks that `pid`, the process serving the socket, is the daemon job.
    ///
    /// Returns the observed mismatch as a readiness detail otherwise.
    async fn served_by_job(&self, pid: Pid) -> Result<(), String> {
        let observation = self.backend.daemon().inspect().await.map_err(|error| {
            format!(
                "daemon socket is served by pid {pid}; the daemon job cannot be inspected: {error}"
            )
        })?;
        match (observation.state, observation.process) {
            (ServiceState::Running, Some(process)) if process.pid == pid => Ok(()),
            (ServiceState::Running, Some(process)) => Err(format!(
                "daemon socket is served by pid {pid}; the supervised job runs pid {}",
                process.pid
            )),
            (ServiceState::Running, None) => Err(format!(
                "daemon socket is served by pid {pid}; the supervised job has no process"
            )),
            (state, _) => Err(format!(
                "daemon socket is served by pid {pid}; the supervised job is {}",
                state_name(state)
            )),
        }
    }

    /// Starts an installed daemon that is not running, then waits for it.
    async fn ensure_daemon(
        &self,
        definition: &JobDefinition,
        version: &str,
        report: &mut UninstallReport,
    ) -> Result<(), Error> {
        let daemon = self.backend.daemon();
        match daemon.inspect().await {
            Ok(observation) if observation.state == ServiceState::Running => {}
            Ok(_stopped) => {
                daemon
                    .replace(definition)
                    .await
                    .map_err(|source| supervisor_error("start daemon", source))?;
                report.started_daemon = true;
            }
            Err(supervisor::Error::NotFound(_)) => {
                daemon
                    .install(definition)
                    .await
                    .map_err(|source| supervisor_error("start daemon", source))?;
                report.started_daemon = true;
            }
            Err(source) => return Err(supervisor_error("inspect daemon", source)),
        }
        self.wait_ready(version).await
    }

    /// Returns live sessions and live worker runtimes not covered by them.
    async fn blocking(
        &self,
        sessions: &[SessionInfo],
    ) -> Result<(Vec<LiveSession>, Vec<String>), Error> {
        let live: Vec<LiveSession> = sessions
            .iter()
            .filter(|session| is_live(session))
            .map(|session| LiveSession {
                id: session.id.0.clone(),
                name: session.name.clone(),
            })
            .collect();
        let journals = Journals::scan(self.context.paths())?;
        if !journals.unreadable.is_empty() {
            return Err(Error::UnreadableJournals {
                paths: journals.unreadable,
            });
        }
        let outdated = journals.outdated_live(&*self.inspector);
        if !outdated.is_empty() {
            return Err(Error::OutdatedJournals {
                workers: outdated
                    .into_iter()
                    .map(|journal| OutdatedWorker {
                        path: journal.path.clone(),
                        schema_version: journal.schema_version,
                        pid: journal.worker_pid,
                    })
                    .collect(),
            });
        }
        let covered = |session: &str| live.iter().any(|live| live.id == session);
        let mut workers = Vec::new();
        for observation in self.discover().await? {
            let Ok(key) = WorkerKey::from_service_id(&observation.id) else {
                continue;
            };
            if covered(key.session_id()) || !job_alive(&observation) {
                continue;
            }
            if journals.final_for(key.session_id(), key.generation()) != Some(true) {
                workers.push(observation.id.to_string());
            }
        }
        for journal in journals.live(&*self.inspector) {
            let label = format!("{}.{}", journal.session_id, journal.generation);
            if !covered(&journal.session_id) && !workers.contains(&label) {
                workers.push(label);
            }
        }
        workers.sort();
        Ok((live, workers))
    }

    /// Waits until no session, worker job, or journal is live any more.
    async fn wait_settled(&self) -> Result<(), Error> {
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            // A failed listing is not proof of anything; keep waiting.
            let (live, workers) = match self.backend.control().sessions().await {
                Ok(sessions) => self.blocking(&sessions).await?,
                Err(error) => (
                    Vec::new(),
                    vec![format!("daemon session list unavailable: {error}")],
                ),
            };
            if live.is_empty() && workers.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::StopTimeout {
                    sessions: live,
                    workers,
                });
            }
            tokio::time::sleep(settings::POLL_INTERVAL).await;
        }
    }

    /// Retires every remaining worker job after proving its runtime ended.
    async fn retire_ended_workers(&self) -> Result<Vec<String>, Error> {
        let (live, workers) = self.blocking(&[]).await?;
        if !live.is_empty() || !workers.is_empty() {
            return Err(Error::OrphanWorkers { ids: workers });
        }
        let mut retired = Vec::new();
        for observation in self.discover().await? {
            match self.backend.workers().retire(&observation.id).await {
                Ok(()) | Err(supervisor::Error::NotFound(_)) => {
                    retired.push(observation.id.to_string());
                }
                Err(source) => return Err(supervisor_error("retire worker", source)),
            }
        }
        let remaining: Vec<String> = self
            .discover()
            .await?
            .iter()
            .map(|observation| observation.id.to_string())
            .collect();
        if remaining.is_empty() {
            Ok(retired)
        } else {
            Err(Error::OrphanWorkers { ids: remaining })
        }
    }

    /// Discovers worker jobs for destructive decisions.
    ///
    /// The strict discovery refuses a result that may omit a raced job, so
    /// an uninstall never retires around a job it cannot see.
    async fn discover(&self) -> Result<Vec<ServiceObservation>, Error> {
        self.backend
            .workers()
            .discover_strict()
            .await
            .map_err(|source| supervisor_error("discover workers", source))
    }

    /// Removes backend directories, the CLI copy, unreferenced versions, and
    /// the prefix ownership record.
    ///
    /// The CLI copy goes before the versions: it is recognized only by its
    /// match with a version directory's CLI. The ownership record goes last,
    /// so a rerun after a failure still owns the prefix. It is released only
    /// when no version is kept for a reference only this namespace can see;
    /// another namespace's collection honors every other reason.
    async fn remove_installation(
        &self,
        layout: &InstallLayout,
        namespace: &Namespace,
        report: &mut UninstallReport,
    ) -> Result<(), Error> {
        let paths = self.context.paths();
        for directory in [paths.launchd_definitions_dir(), paths.launchd_log_dir()] {
            if remove_child_dir(&directory)? {
                report.removed.push(directory);
            }
        }
        if layout::installed_cli_copy(layout)? {
            layout::remove_cli(layout)?;
            report.removed.push(layout.bin_dir().join(layout::CLI_NAME));
        }
        // Re-check references right before deleting: a worker started since
        // the settlement check keeps its version.
        let usage = self.usage(layout).await?;
        let mut namespace_bound = false;
        for version in layout::installed_versions(layout)? {
            match usage.keep_reason(&version, None) {
                Some(reason) => {
                    namespace_bound |= usage.namespace_bound(&version);
                    report.kept_versions.push(KeptVersion { version, reason });
                }
                None => {
                    if layout::remove_version(layout, &version)? {
                        report.removed.push(
                            layout
                                .version_dir(&version)
                                .expect("listed versions are valid"),
                        );
                    }
                }
            }
        }
        if !namespace_bound && layout::release_prefix(layout, namespace)? {
            report.removed.push(layout::owner_record(layout));
        }
        Ok(())
    }

    /// Purges durable metadata when no installation's daemon is registered.
    ///
    /// Applies the uninstall's own guards: a registered daemon job may still
    /// serve the session store, and a worker job is retired only after its
    /// runtime is proven ended, since the purge deletes the journals that prove it.
    async fn purge_unregistered(&self, report: &mut UninstallReport) -> Result<(), Error> {
        match self.backend.daemon().inspect().await {
            Err(supervisor::Error::NotFound(_)) => {}
            Ok(observation) => {
                return Err(Error::DaemonJobPresent {
                    id: observation.id.to_string(),
                });
            }
            Err(source) => return Err(supervisor_error("inspect daemon", source)),
        }
        report.retired_workers = self.retire_ended_workers().await?;
        self.purge(report)?;
        report.purged = true;
        Ok(())
    }

    /// Removes the session store, event logs, worker journals, and host identity.
    fn purge(&self, report: &mut UninstallReport) -> Result<(), Error> {
        let paths = self.context.paths();
        if let Some(data) = layout::open_existing_owner_dir(&paths.data_dir)? {
            for name in PURGED_DATA_FILES {
                if file_exists(&paths.data_dir.join(name))? {
                    record::remove_file(&data, name)?;
                    report.removed.push(paths.data_dir.join(name));
                }
            }
            for name in PURGED_DATA_DIRS {
                if layout::remove_tree(&data, name)? {
                    report.removed.push(paths.data_dir.join(name));
                }
            }
        }
        if let Some(state) = layout::open_existing_owner_dir(&paths.state_dir)? {
            for name in PURGED_STATE_DIRS {
                if layout::remove_tree(&state, name)? {
                    report.removed.push(paths.state_dir.join(name));
                }
            }
        }
        Ok(())
    }

    /// Runs [`Self::collect_garbage`] for an upgrade report.
    ///
    /// The upgrade itself already succeeded, so a collection failure is
    /// reported beside it instead of failing the command.
    async fn collect_garbage_reported(
        &self,
        layout: &InstallLayout,
        active: &str,
    ) -> (Vec<String>, Vec<KeptVersion>, Option<String>) {
        match self.collect_garbage(layout, active).await {
            Ok((removed, kept)) => (removed, kept, None),
            Err(error) => (Vec::new(), Vec::new(), Some(error.to_string())),
        }
    }

    /// Deletes version directories nothing references any more.
    async fn collect_garbage(
        &self,
        layout: &InstallLayout,
        active: &str,
    ) -> Result<(Vec<String>, Vec<KeptVersion>), Error> {
        // The usage below knows only this namespace's journals and jobs.
        layout::claim_existing_prefix(layout, &self.context.namespace()?)?;
        let usage = self.usage(layout).await?;
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for version in layout::installed_versions(layout)? {
            if let Some(reason) = usage.keep_reason(&version, Some(active)) {
                kept.push(KeptVersion { version, reason });
            } else {
                layout::remove_version(layout, &version)?;
                removed.push(version);
            }
        }
        Ok((removed, kept))
    }

    /// Collects what references each version, discovering worker jobs.
    ///
    /// The discovery is strict: a raced job may be missing from the result,
    /// and a version its job still executes must not be deleted. A failed
    /// discovery is recorded in the result, which then keeps every version.
    async fn usage(&self, layout: &InstallLayout) -> Result<Usage, Error> {
        let journals = Journals::scan(self.context.paths())?;
        let discovered = self.backend.workers().discover_strict().await;
        Ok(self.collect_usage(
            layout,
            &journals,
            discovered.as_deref().map_err(ToString::to_string),
        ))
    }

    /// Collects what references each version from already gathered facts.
    fn collect_usage(
        &self,
        layout: &InstallLayout,
        journals: &Journals,
        discovered: Result<&[ServiceObservation], String>,
    ) -> Usage {
        let mut usage = Usage::collect(
            layout,
            journals,
            &*self.inspector,
            self.context.cli_executable(),
        );
        usage.note_jobs(layout, discovered);
        usage
    }
}

/// Creates the launchd log directory the daemon agent writes into.
#[cfg(target_os = "macos")]
fn layout_log_dir(context: &Context) -> Result<(), Error> {
    /// launchd opens the log files but never creates their directory.
    const LOG_DIR_MODE: u32 = 0o700;

    pohunek_platform::filesystem::TrustedDir::open_or_create_absolute(
        context.paths().launchd_log_dir(),
        LOG_DIR_MODE,
    )
    .map(drop)
    .map_err(|source| super::error::fs_error("create launchd log directory", source))
}

/// Returns whether a session may still own a live PTY.
fn is_live(session: &SessionInfo) -> bool {
    if session.external == Some(true) {
        return false;
    }
    let runtime_live = session.runtime.as_ref().is_some_and(|runtime| {
        matches!(
            runtime.state,
            RuntimeState::Starting
                | RuntimeState::Live
                | RuntimeState::Reconnecting
                | RuntimeState::Conflict
                | RuntimeState::Incompatible
        )
    });
    runtime_live
        || matches!(
            session.state,
            SessionState::Starting | SessionState::Running
        )
}

/// Returns whether a worker job may still own a live PTY.
///
/// A process proves it directly, and `Starting`, `Running`, and `Stopping`
/// prove it through the service manager. `Unknown` also counts: launchd
/// reports a loaded job without a matching process as `Unknown`, and such a
/// job may still be waiting to spawn. Only `Stopped` and `Failed` prove the
/// end without further evidence; a caller that needs more proof reads the
/// worker journal.
fn job_alive(observation: &ServiceObservation) -> bool {
    observation.process.is_some()
        || matches!(
            observation.state,
            ServiceState::Starting
                | ServiceState::Running
                | ServiceState::Stopping
                | ServiceState::Unknown
        )
}

pub(crate) fn pending_report(record: &Record) -> PendingReport {
    PendingReport {
        operation: record.operation.as_str(),
        version: record.version.clone(),
        step: record.step.as_str(),
    }
}

/// What an install or upgrade does with the pending transaction record.
#[derive(Debug)]
pub(crate) enum Plan {
    /// Nothing is pending.
    Fresh,
    /// The pending record is this transaction; it resumes after its step.
    Resume(Record),
    /// The pending record is another transaction, rolled back first.
    RollBack(Record),
}

impl Plan {
    /// The record the plan resumes or rolls back.
    pub(crate) fn record(&self) -> Option<&Record> {
        match self {
            Self::Fresh => None,
            Self::Resume(record) | Self::RollBack(record) => Some(record),
        }
    }
}

/// Everything install checks before its first effect.
///
/// Refuses an untrusted unit or agent directory, a prefix `service.toml`
/// would reject, an untrusted directory install writes below the prefix or
/// `service.toml` into, and a `HOME` or XDG root the daemon's job definition
/// cannot carry. `pohunek service check` runs exactly these checks.
///
/// # Errors
///
/// Returns [`Error::InvalidPath`], [`Error::UntrustedDirectory`], or the
/// environment errors of [`Context::service_environment`].
pub(crate) fn install_preflight(context: &Context, prefix: &Path) -> Result<InstallLayout, Error> {
    // The environment goes first: on macOS the agent directory lies below
    // `HOME`, so an unusable `HOME` is named as such rather than as the
    // directory it breaks.
    context.service_environment()?;
    layout::check_trusted(context.supervisor_dir())?;
    let layout = install_layout(prefix)?;
    check_layout_dirs(context, &layout)?;
    Ok(layout)
}

/// Reads `service.toml` for an install resumed past the config step.
///
/// The file is proven to describe this installation before any further
/// effect: its prefix, version, and namespace inputs must equal those of the
/// interrupted install and of the current user. A mismatch is an error; the
/// file is never overwritten here. `service check` runs this same function.
///
/// # Errors
///
/// Returns the load errors of [`ServiceConfig::load`] for a missing or
/// invalid file, [`Error::Config`] for another namespace, and
/// [`Error::ResumeConfigMismatch`] for another prefix or version.
pub(crate) fn verify_resume_config(
    context: &Context,
    prefix: &Path,
    version: &str,
) -> Result<ServiceConfig, Error> {
    let config = ServiceConfig::load(&context.config_path())?;
    let (state_root, runtime_root) = context.roots()?;
    config.verify_installation(context.uid(), &state_root, &runtime_root)?;
    if config.prefix() != prefix {
        return Err(Error::ResumeConfigMismatch {
            key: "prefix",
            recorded: config.prefix().display().to_string(),
            expected: prefix.display().to_string(),
        });
    }
    if config.active_version() != version {
        return Err(Error::ResumeConfigMismatch {
            key: "active_version",
            recorded: config.active_version().to_owned(),
            expected: version.to_owned(),
        });
    }
    Ok(config)
}

/// Resolves the daemon `PATH` when the install plan will write `service.toml`.
///
/// A fresh install, a rolled-back one, and a resume before the config step
/// resolve it; a resume after that step keeps the recorded path. The discovery
/// environment and a missing trusted directory fail here, before any effect.
/// `pohunek service check` runs the same function.
///
/// # Errors
///
/// Returns the environment errors and [`Error::SearchPath`] of
/// [`Context::install_search_path`].
pub(crate) fn discover_for_plan(
    context: &Context,
    plan: &Plan,
) -> Result<Option<(SearchPath, SearchPathReport)>, Error> {
    plan_discovers(plan)
        .then(|| context.install_search_path())
        .transpose()
}

/// Whether `plan` writes `service.toml` and so resolves the daemon `PATH`.
pub(crate) fn plan_discovers(plan: &Plan) -> bool {
    match plan {
        Plan::Fresh | Plan::RollBack(_) => true,
        Plan::Resume(record) => record.step < Step::Config,
    }
}

/// Everything upgrade checks before it reads `service.toml`.
///
/// The installed prefix is checked with [`check_layout_dirs`] once
/// `service.toml` names it.
///
/// # Errors
///
/// See [`install_preflight`].
pub(crate) fn upgrade_preflight(context: &Context) -> Result<(), Error> {
    context.service_environment()?;
    layout::check_trusted(context.supervisor_dir())
}

/// Checks every directory install or upgrade writes into.
///
/// Those are the prefix, `<prefix>/bin`, `<prefix>/libexec`, the versions
/// directory, and the directory of `service.toml`; a missing one is judged
/// by its nearest existing ancestor, below which it will be created.
///
/// # Errors
///
/// Returns [`Error::UntrustedDirectory`] naming the offending directory.
pub(crate) fn check_layout_dirs(context: &Context, layout: &InstallLayout) -> Result<(), Error> {
    let versions = layout.versions_dir();
    let bin = layout.bin_dir();
    let config_path = context.config_path();
    let directories = [
        Some(layout.prefix()),
        Some(bin.as_path()),
        versions.parent(),
        Some(versions.as_path()),
        config_path.parent(),
    ];
    for directory in directories.into_iter().flatten() {
        layout::check_trusted(directory)?;
    }
    Ok(())
}

/// Refuses an install that starts over while a daemon job is registered.
///
/// Without `service.toml` nothing describes such a job, so registering
/// another daemon beside it could never be undone safely. Install runs this
/// before registering anything, and `pohunek service check` runs it too.
///
/// # Errors
///
/// Returns [`Error::DaemonJobPresent`] for a registered daemon job and
/// [`Error::Supervisor`] when the service manager cannot be asked.
pub(crate) async fn ensure_no_daemon_job(backend: &Backend) -> Result<(), Error> {
    match backend.daemon().inspect().await {
        Err(supervisor::Error::NotFound(_)) => Ok(()),
        Ok(observation) => Err(Error::DaemonJobPresent {
            id: observation.id.to_string(),
        }),
        Err(source) => Err(supervisor_error("inspect daemon", source)),
    }
}

/// Decides what an install of `version` into `prefix` does with `pending`.
///
/// # Errors
///
/// Returns [`Error::PendingInstall`] for another install that reached the
/// registration step: it may already run its daemon with live workers, so
/// only its own `install` may finish it and only the uninstall flow's
/// session checks may remove it.
pub(crate) fn install_plan(
    pending: Option<Record>,
    prefix: &Path,
    version: &str,
) -> Result<Plan, Error> {
    match pending {
        Some(record)
            if record.operation == Operation::Install
                && record.version == version
                && record.prefix == prefix
                && !record.rolling_back =>
        {
            Ok(Plan::Resume(record))
        }
        Some(record)
            if record.operation == Operation::Install && record.step >= Step::Registering =>
        {
            Err(Error::PendingInstall {
                version: record.version,
                step: record.step.as_str(),
            })
        }
        Some(record) => Ok(Plan::RollBack(record)),
        None => Ok(Plan::Fresh),
    }
}

/// Decides what an upgrade to `version` does with `pending`.
///
/// # Errors
///
/// Returns [`Error::PendingInstall`] for any pending install: rolling it
/// back would remove its daemon, which runs the new version once the record
/// reached `ready`.
pub(crate) fn upgrade_plan(pending: Option<Record>, version: &str) -> Result<Plan, Error> {
    match pending {
        Some(record) if record.operation == Operation::Install => Err(Error::PendingInstall {
            version: record.version,
            step: record.step.as_str(),
        }),
        // A rolling-back record's `config` and later steps may already be
        // undone, so resuming would skip them; the rollback is finished first
        // and the upgrade starts over.
        Some(record) if record.version == version && !record.rolling_back => {
            Ok(Plan::Resume(record))
        }
        Some(record) => Ok(Plan::RollBack(record)),
        None => Ok(Plan::Fresh),
    }
}

/// Validates `prefix` with the exact rule [`ServiceConfig::new`] applies.
///
/// Install and rollback check the prefix before any effect, so a prefix the
/// configuration would reject never lets a pending transaction be rolled
/// back first.
fn install_layout(prefix: &Path) -> Result<InstallLayout, Error> {
    pohunek_service_config::validate_prefix(prefix).map_err(|_invalid| Error::InvalidPath {
        flag: "--prefix",
        path: prefix.to_path_buf(),
    })
}

/// Loads `service.toml`, or `None` when it does not exist.
fn load_config(path: &Path) -> Result<Option<ServiceConfig>, Error> {
    if file_exists(path)? {
        Ok(Some(ServiceConfig::load(path)?))
    } else {
        Ok(None)
    }
}

fn file_exists(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_metadata) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(super::error::io_error("inspect", path)(error)),
    }
}

/// Removes `service.toml`; returns whether it existed.
fn remove_config(path: &Path) -> Result<bool, Error> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(false);
    };
    let Some(directory) = layout::open_existing_owner_dir(parent)? else {
        return Ok(false);
    };
    let existed = file_exists(path)?;
    record::remove_file(&directory, name)?;
    Ok(existed)
}

/// Removes a directory tree given by its absolute path; returns whether it existed.
fn remove_child_dir(path: &Path) -> Result<bool, Error> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(false);
    };
    match layout::open_existing_owner_dir(parent)? {
        Some(directory) => layout::remove_tree(&directory, name),
        None => Ok(false),
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
