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
//! 5. `ready`: the daemon answers `daemon.health` as the new version;
//! 6. `cli`: `<prefix>/bin/pohunek` becomes the new CLI.
//!
//! A failing step rolls the transaction back: the daemon job is removed
//! (install) or replaced by the previous version (upgrade), `service.toml`
//! is removed or restored, and a version directory this transaction created
//! is removed unless something references it. A failure after `ready` keeps
//! the record instead, because the service already runs the new version; the
//! next run finishes the CLI step.
//!
//! An interrupted process leaves the record behind. The next `install` or
//! `upgrade` of the same version and prefix resumes after the recorded step;
//! any other operation, including `uninstall`, rolls it back first. The one
//! exception is an `upgrade` meeting a pending install: it fails with
//! `service_install_pending` and touches nothing, because only `install`
//! may finish or roll back an install whose daemon may already run the new
//! version. Every step is idempotent, so resuming never duplicates a job or a
//! directory.
//!
//! After a successful upgrade, version directories that are neither active,
//! referenced by the journal of a worker that may still run, named by a
//! registered worker job, executed by a running process, nor holding the
//! running CLI are removed. When worker jobs cannot be discovered, every
//! version is kept.
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
//!
//! # Uninstall
//!
//! The daemon must enumerate sessions, so an installed but stopped daemon is
//! started through the service manager first. Live sessions refuse the
//! uninstall unless `--stop-sessions` is given; then each is stopped through
//! the daemon and the engine waits until every worker's journal is final and
//! its process is gone. Only then are the daemon job, the remaining ended
//! worker jobs, the launchd directories, the installed `<prefix>/bin/pohunek`
//! copy, and unreferenced version directories removed. `--purge` also
//! removes the session store, event logs, worker journals, and host
//! identity. `service.toml` goes last: it names the prefix, so while any
//! earlier removal fails, a rerun of `uninstall` still finds the
//! installation and finishes the cleanup.

// Rust guideline compliant 2026-09-25

use std::path::Path;
use std::time::{Duration, Instant};

use pohunek_paths::InstallLayout;
use pohunek_platform::process::HostInspector;
use pohunek_platform::supervisor::{
    self, JobDefinition, ServiceObservation, ServiceState, WorkerKey,
};
use pohunek_service_config::ServiceConfig;
use protocol::{RuntimeState, SessionInfo, SessionState};

use super::backend::Backend;
use super::context::Context;
use super::definition::{daemon_definition, initial_config, with_version};
use super::error::{supervisor_error, Error, LiveSession};
use super::layout::{self, Staged};
use super::record::{self, Operation, Record, Step, Store};
use super::report::{
    InstallReport, JobReport, KeptVersion, PendingReport, StatusReport, UninstallReport,
    UpgradeReport, VersionReport, WorkerReport,
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
    inspector: HostInspector,
    store: Store,
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
            inspector: HostInspector::new(),
            store: Store::new(context.paths().state_dir.clone()),
            ready_timeout: settings::DAEMON_READY_TIMEOUT,
            stop_timeout: settings::SESSION_STOP_TIMEOUT,
            #[cfg(test)]
            interrupt_after: None,
            #[cfg(feature = "test-util")]
            reported_version: None,
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
    /// Returns [`Error::AlreadyInstalled`] when `service.toml` exists,
    /// [`Error::DaemonJobPresent`] for a stale daemon job, staging and probe
    /// errors, and the failing step's error after a rollback.
    pub async fn install(
        &self,
        from: &Path,
        prefix: &Path,
        version: &str,
    ) -> Result<InstallReport, Error> {
        let _transaction = self.store.lock().await?;
        layout::check_trusted(self.context.supervisor_dir())?;
        layout::check_trusted(prefix)?;
        let layout = install_layout(prefix)?;
        let pending = self.store.load()?;
        let (resume, rolled_back) = match pending {
            Some(record)
                if record.operation == Operation::Install
                    && record.version == version
                    && record.prefix == prefix =>
            {
                (Some(record), None)
            }
            Some(record) => {
                let report = pending_report(&record);
                self.rollback_pending(&record).await?;
                (None, Some(report))
            }
            None => (None, None),
        };
        let config_path = self.context.config_path();
        if resume.is_none() {
            if file_exists(&config_path)? {
                return Err(Error::AlreadyInstalled { path: config_path });
            }
            match self.backend.daemon().inspect().await {
                Err(supervisor::Error::NotFound(_)) => {}
                Ok(observation) => {
                    return Err(Error::DaemonJobPresent {
                        id: observation.id.to_string(),
                    });
                }
                Err(source) => return Err(supervisor_error("inspect daemon", source)),
            }
        }
        let config = initial_config(self.context, prefix, version)?;
        let staged = layout::stage(&layout, from, self.reported(version)).await?;
        let resumed = resume.is_some();
        let mut record = if let Some(record) = resume {
            record
        } else {
            {
                let record = Record {
                    schema_version: record::SCHEMA_VERSION,
                    operation: Operation::Install,
                    version: version.to_owned(),
                    prefix: prefix.to_path_buf(),
                    previous_version: None,
                    version_dir_preexisted: layout::version_exists(&layout, version)?,
                    step: Step::Started,
                };
                self.store.save(&record)?;
                record
            }
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
        })
    }

    /// Upgrades the installation to `version` from the binaries in `from`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PendingInstall`] while an interrupted install's record
    /// exists, [`Error::NotInstalled`] without `service.toml`, staging and
    /// probe errors, and the failing step's error after a rollback.
    pub async fn upgrade(&self, from: &Path, version: &str) -> Result<UpgradeReport, Error> {
        let _transaction = self.store.lock().await?;
        layout::check_trusted(self.context.supervisor_dir())?;
        let config_path = self.context.config_path();
        let pending = self.store.load()?;
        let (resume, rolled_back) = match pending {
            // Rolling a pending install back would remove its daemon, which
            // runs the new version once the record reached `ready`.
            Some(record) if record.operation == Operation::Install => {
                return Err(Error::PendingInstall {
                    version: record.version,
                    step: record.step.as_str(),
                });
            }
            Some(record) if record.version == version => (Some(record), None),
            Some(record) => {
                let report = pending_report(&record);
                self.rollback_pending(&record).await?;
                (None, Some(report))
            }
            None => (None, None),
        };
        let config = load_config(&config_path)?.ok_or(Error::NotInstalled {
            path: config_path.clone(),
        })?;
        let layout = config.layout().clone();
        layout::check_trusted(layout.prefix())?;
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
            return Ok(UpgradeReport {
                from_version,
                to_version: version.to_owned(),
                unchanged: true,
                resumed: false,
                rolled_back,
                removed_versions: Vec::new(),
                kept_versions: Vec::new(),
                gc_error: None,
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
                };
                self.store.save(&record)?;
                record
            }
        };
        let target = with_version(&config, version)?;
        let result = self.run_steps(&mut record, &layout, staged, &target).await;
        self.settle(&record, result).await?;
        let (removed_versions, kept_versions, gc_error) =
            match self.collect_garbage(&layout, version).await {
                Ok((removed, kept)) => (removed, kept, None),
                Err(error) => (Vec::new(), Vec::new(), Some(error.to_string())),
            };
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
    /// Returns [`Error::NotInstalled`] when nothing is installed,
    /// [`Error::LiveSessions`] when sessions are live without
    /// `--stop-sessions`, [`Error::StopTimeout`] when they do not settle, and
    /// [`Error::OrphanWorkers`] when worker jobs survive retirement.
    pub async fn uninstall(&self, options: UninstallOptions) -> Result<UninstallReport, Error> {
        let _transaction = self.store.lock().await?;
        let mut report = UninstallReport::default();
        if let Some(pending) = self.store.load()? {
            report.rolled_back = Some(pending_report(&pending));
            self.rollback_pending(&pending).await?;
        }
        let config_path = self.context.config_path();
        let Some(config) = load_config(&config_path)? else {
            if report.rolled_back.is_some() {
                return Ok(report);
            }
            return Err(Error::NotInstalled { path: config_path });
        };
        let layout = config.layout().clone();
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

        self.remove_installation(&layout, &mut report).await?;
        if options.purge {
            self.purge(&mut report)?;
            report.purged = true;
        }
        if remove_config(&config_path)? {
            report.removed.push(config_path);
        }
        self.store.clear()?;
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
    async fn rollback(&self, record: &Record) -> Result<(), Error> {
        let layout = install_layout(&record.prefix)?;
        let config_path = self.context.config_path();
        match record.operation {
            Operation::Install => {
                if record.step >= Step::Registering {
                    self.backend
                        .daemon()
                        .uninstall()
                        .await
                        .map_err(|source| supervisor_error("uninstall daemon", source))?;
                }
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
        if !record.version_dir_preexisted {
            let usage = self.usage(&layout).await?;
            if usage.keep_reason(&record.version, None).is_none() {
                layout::remove_version(&layout, &record.version)?;
            }
        }
        Ok(())
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

    /// Waits until the daemon answers `daemon.health` as `version`.
    async fn wait_ready(&self, version: &str) -> Result<(), Error> {
        let deadline = Instant::now() + self.ready_timeout;
        loop {
            let mut last = match self.backend.control().health().await {
                Ok(health) if health.daemon_version == self.reported(version) => return Ok(()),
                Ok(health) => Some(format!("daemon reports version {}", health.daemon_version)),
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
        for journal in journals.live(&self.inspector) {
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

    async fn discover(&self) -> Result<Vec<ServiceObservation>, Error> {
        self.backend
            .workers()
            .discover()
            .await
            .map_err(|source| supervisor_error("discover workers", source))
    }

    /// Removes backend directories, the CLI copy, and unreferenced versions.
    ///
    /// The CLI copy goes before the versions: it is recognized only by its
    /// match with a version directory's CLI.
    async fn remove_installation(
        &self,
        layout: &InstallLayout,
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
        for version in layout::installed_versions(layout)? {
            match usage.keep_reason(&version, None) {
                Some(reason) => report.kept_versions.push(KeptVersion { version, reason }),
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

    /// Deletes version directories nothing references any more.
    async fn collect_garbage(
        &self,
        layout: &InstallLayout,
        active: &str,
    ) -> Result<(Vec<String>, Vec<KeptVersion>), Error> {
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
    /// A failed discovery is recorded in the result, which then keeps every
    /// version.
    async fn usage(&self, layout: &InstallLayout) -> Result<Usage, Error> {
        let journals = Journals::scan(self.context.paths())?;
        let discovered = self.backend.workers().discover().await;
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
            &self.inspector,
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

/// Returns whether a worker job still has a process that may own a PTY.
fn job_alive(observation: &ServiceObservation) -> bool {
    observation.process.is_some()
        || matches!(
            observation.state,
            ServiceState::Starting | ServiceState::Running | ServiceState::Stopping
        )
}

fn pending_report(record: &Record) -> PendingReport {
    PendingReport {
        operation: record.operation.as_str(),
        version: record.version.clone(),
        step: record.step.as_str(),
    }
}

fn install_layout(prefix: &Path) -> Result<InstallLayout, Error> {
    InstallLayout::new(prefix).map_err(|_invalid| Error::InvalidPath {
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
