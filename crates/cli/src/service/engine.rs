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
//! # Config migration
//!
//! A `service.toml` written by a previous release (`schema_version` 2, 3, 4, 5 or 6,
//! in contrast to this crate's [`SCHEMA_VERSION`]) is read only at the
//! upgrade boundary, through `ServiceConfig::load_upgrade_source`, which
//! assigns the documented defaults to the keys that schema lacks (input
//! timings, the write stall bound, turn-wait timings, the `[hooks]` stderr
//! tail size, and the `[events]` journal retention) explicitly. Its upgrade is a dedicated
//! migration flavor of the transaction, journaled in the record's
//! `config_backup` (`source_schema_version` and the SHA-256 of the exact
//! pre-upgrade bytes), with two consequences for ordering:
//!
//! * Before anything rewrites `service.toml`, the migration saves the exact
//!   pre-upgrade bytes to an owner-private backup next to the record (the
//!   `backed_up` step), so a rollback can restore them byte for byte.
//! * A previous daemon cannot read a current-schema file: the moment
//!   `service.toml` is rewritten, its own strict reader would refuse a restart
//!   and its restart policy would crash-loop it. The migration therefore
//!   stops the daemon job (journaling the `stop_daemon` step) after the
//!   adoption preflight and before the rewrite; registering then installs the
//!   new definition.
//!
//! The migration's replay is closed around the backup: on resume, the backup
//! must exist and match the journal's digest, and until the rewrite the file
//! itself must still be the exact pre-upgrade bytes. A rollback first proves
//! the backup and, once the migration stopped the daemon, judges the previous
//! daemon's store reader and live-worker adoption. It then stops a registered
//! replacement, judges the store again, restores the exact bytes, and starts
//! the previous daemon against its own file. A missing or altered backup fails
//! before changing any file or daemon.
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
//! A pending install or upgrade that already reached the registration step
//! takes this same flow instead of a rollback: its daemon may already run it
//! with live workers, and only the session-checked removal above may stop
//! them. For an upgrade this also means the previous daemon is never
//! restored: its reader may already be incompatible with the migrated store
//! (or with the live workers), and only the removal itself, blessed by
//! `--stop-sessions`, decides what happens to the runtimes. Such a record
//! found without `service.toml` describes its installation through
//! its version and prefix, and the uninstall runs the same flow with the
//! configuration the transaction wrote.
//!
//! Worker journals an older pohunek wrote under an earlier schema block the
//! uninstall while their worker may still run. The daemon of this version
//! cannot stop those workers, so the uninstall names them instead of
//! waiting on them.

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pohunek_paths::InstallLayout;
use pohunek_platform::process::{HostInspector, Pid, ProcessInspector};
use pohunek_platform::supervisor::{
    self, JobDefinition, Namespace, ServiceObservation, ServiceState, WorkerKey,
};
use pohunek_service_config::preflight::PreflightReport;
use pohunek_service_config::runtimes::EnsureReport;
use pohunek_service_config::{
    body_after_header, ConfigError, ServiceConfig, UpgradeSource, MAX_CONFIG_BYTES, SCHEMA_VERSION,
};
use protocol::SessionInfo;

use super::backend::Backend;
use super::context::Context;
use super::definition::{
    config_with_search_path, daemon_definition, identity_config, unknown_keys_warning, with_version,
};
use super::error::{supervisor_error, Error, LiveSession, OutdatedWorker};
use super::layout::{self, Staged, VerifiedVersionDir};
use pohunek_platform::shell_env::SearchPath;

use super::ledger::{Acceptance, Ledger};
use super::manager_env;
use super::preflight::{self, AdoptionPreflight, DaemonPreflight, Gated};
use super::record::{self, ConfigBackup, Operation, Record, Step, Store, TransactionLock};
use super::report::{
    state_name, InstallReport, JobReport, KeptVersion, PendingReport, SearchPathReport,
    StatusReport, UninstallReport, UpgradeReport, VersionReport, WorkerReport,
};
use super::report::{AcceptanceGroups, IntegrationReinstall};
use super::rollback;
use super::runtimes::{self, CliIntegrations, DaemonRuntimes, IntegrationStep, RuntimeStep};
use super::settings;
use super::usage::{Journals, Usage};
#[cfg(target_os = "linux")]
use super::verify::UnitVerifier;
use crate::commands::integration::IntegrationEnv;
use crate::paths::Paths;

/// Durable metadata `--purge` removes from the data directory.
///
/// Names match the daemon's store (`metadata.jsonl`) and event log
/// directory (`events`). Worktrees, notifications, and configuration are
/// never purged: they may hold the owner's work. The store's pre-migration
/// backups (`pohunek_paths::is_schema_backup_artifact`) are full copies of it
/// and are purged with it, alongside crashed store-write copies and imported
/// legacy-manifest archives. Unrecognized data entries are retained and reported.
const PURGED_DATA_FILES: [&str; 1] = [pohunek_paths::METADATA_STORE_NAME];

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

/// What the adoption gate right before the daemon swap left for the caller.
#[derive(Debug, Default)]
struct LateGate {
    /// The preflight that let the swap proceed.
    gated: Option<Gated>,
    /// Whether the gate itself failed, which leaves the daemon untouched.
    refused: bool,
    /// The runtime plan, or the apply, this run carried out.
    runtimes: Option<EnsureReport>,
    /// Conditions of this run's inputs that the operator must be told.
    warnings: Vec<String>,
}

/// A test action run just before the adoption preflight of the daemon swap.
#[cfg(test)]
struct SwapHook(Box<dyn Fn() + Send + Sync>);

#[cfg(feature = "test-util")]
struct RollbackStopHook(Box<dyn Fn() + Send + Sync>);

#[cfg(feature = "test-util")]
impl std::fmt::Debug for RollbackStopHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RollbackStopHook(..)")
    }
}

#[cfg(test)]
impl std::fmt::Debug for SwapHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SwapHook(..)")
    }
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
    /// Judges whether the new daemon adopts the live sessions; the new
    /// daemon's own `upgrade-preflight` in production.
    preflight: Box<dyn AdoptionPreflight>,
    /// Plans and applies the release's official runtime packages; the
    /// release's own `pohunekd ensure-official-runtimes` in production.
    runtimes: Box<dyn RuntimeStep>,
    /// Reinstalls the integration hooks the release lists once the new daemon serves.
    integrations: Box<dyn IntegrationStep>,
    /// Whether the operator accepted losing the runtime or native recovery of
    /// the sessions the preflight lists (`--accept-runtime-loss`).
    accept_runtime_loss: bool,
    /// Verifies the rendered units before they are registered.
    #[cfg(target_os = "linux")]
    unit_verifier: UnitVerifier,
    #[cfg(test)]
    interrupt_after: Option<Step>,
    /// Runs between the last step before the daemon swap and its adoption
    /// preflight, standing in for the old daemon accepting work meanwhile.
    #[cfg(test)]
    before_swap: Option<SwapHook>,
    #[cfg(feature = "test-util")]
    reported_version: Option<String>,
    #[cfg(feature = "test-util")]
    after_rollback_stop: Option<RollbackStopHook>,
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
            preflight: Box::new(DaemonPreflight),
            runtimes: Box::new(DaemonRuntimes::default()),
            integrations: Box::new(CliIntegrations),
            accept_runtime_loss: false,
            #[cfg(target_os = "linux")]
            unit_verifier: UnitVerifier::default(),
            #[cfg(test)]
            interrupt_after: None,
            #[cfg(test)]
            before_swap: None,
            #[cfg(feature = "test-util")]
            reported_version: None,
            #[cfg(feature = "test-util")]
            after_rollback_stop: None,
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

    /// Lets an upgrade proceed over the sessions its adoption preflight lists
    /// as at risk (`--accept-runtime-loss`).
    ///
    /// A store the new daemon cannot start with is refused either way.
    #[must_use]
    pub fn with_runtime_loss_accepted(mut self, accepted: bool) -> Self {
        self.accept_runtime_loss = accepted;
        self
    }

    /// Injects a fixture change after the native rollback stop and before
    /// the second compatibility judgment.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn with_after_rollback_stop(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.after_rollback_stop = Some(RollbackStopHook(Box::new(hook)));
        self
    }

    /// Judges adoption with `preflight` instead of the new daemon's
    /// `upgrade-preflight` process.
    ///
    /// Test-only: an alternative judge could approve sessions the new daemon
    /// would lose, so release builds cannot reach it.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_adoption_preflight(mut self, preflight: Box<dyn AdoptionPreflight>) -> Self {
        self.preflight = preflight;
        self
    }

    /// Plans and applies the official runtime packages with `runtimes` instead
    /// of the release's own `pohunekd ensure-official-runtimes`.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_runtime_step(mut self, runtimes: Box<dyn RuntimeStep>) -> Self {
        self.runtimes = runtimes;
        self
    }

    /// Reinstalls the integration hooks with `integrations` instead of the CLI's.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn with_integration_step(mut self, integrations: Box<dyn IntegrationStep>) -> Self {
        self.integrations = integrations;
        self
    }

    /// Verifies the rendered units with `verifier` instead of `systemd-analyze`
    /// found on `PATH`.
    ///
    /// The units are still rendered and verified before registration; only the
    /// program that checks them is configured.
    ///
    /// Test-only: a verifier that accepts everything would let a caller
    /// register units nothing checked, so release builds cannot reach it.
    #[cfg(all(target_os = "linux", any(test, feature = "test-util")))]
    #[must_use]
    pub fn with_unit_verifier(mut self, verifier: UnitVerifier) -> Self {
        self.unit_verifier = verifier;
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

    /// Returns whether `daemon_version` is a version `serving` accepts.
    ///
    /// Every comparison runs through [`Engine::reported`], so a reported
    /// build name judges the same for the expectation and for the daemon.
    fn serving_matches(&self, daemon_version: &str, serving: &Serving<'_>) -> bool {
        match serving {
            Serving::Strict(version) => daemon_version == self.reported(version),
            Serving::Allowed(allowed) => allowed
                .iter()
                .any(|version| daemon_version == self.reported(version)),
        }
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
        let source = layout::operator_release(&layout, from)?;
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
        let (resume, rolled_back) = self.resolve_plan(plan).await?;
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
            let staged = layout::stage(&layout, &source, self.reported(version)).await?;
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
                    config_backup: None,
                    step: Step::Started,
                    rolling_back: false,
                    runtime_stop: false,
                    runtime_backup: None,
                    launchd_default_path: None,
                    launchd_boot_identity: None,
                    accepted_risks: None,
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
        let mut late = LateGate::default();
        let result = self
            .run_steps(&mut record, &layout, staged, &config, &mut late)
            .await;
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
            runtimes: late.runtimes,
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

    /// Applies a pending transaction decision and reports its rollback.
    async fn resolve_plan(
        &self,
        plan: Plan,
    ) -> Result<(Option<Record>, Option<PendingReport>), Error> {
        match plan {
            Plan::Fresh => Ok((None, None)),
            Plan::Resume(record) => Ok((Some(record), None)),
            Plan::RollBack(record) => {
                let mut report = pending_report(&record);
                if let Some(gated) = self.rollback_pending(&record).await? {
                    report.accepted_runtime_loss = gated.accepted;
                    report.warnings.clone_from(&gated.warnings);
                    report.preflight = Some(gated.report);
                }
                Ok((None, Some(report)))
            }
        }
    }

    /// Upgrades the installation to `version` from the binaries in `from`.
    ///
    /// # Errors
    ///
    /// Returns the environment errors [`Self::install`] returns before any
    /// effect, [`Error::PendingInstall`] while an interrupted install's record
    /// exists, [`Error::NotInstalled`] without `service.toml`, staging and
    /// probe errors, the failing step's error after a rollback, and, before
    /// any effect of a daemon swap, [`Error::UpgradeAtRisk`] for live sessions
    /// the new daemon would not adopt (unless the runtime loss is accepted),
    /// [`Error::UpgradeStoreUnusable`] for a store it cannot start with, and
    /// [`Error::UpgradePreflightFailed`] when the preflight gives no report.
    /// A migration transaction additionally fails closed with
    /// [`Error::ConfigBackup`] when this CLI's backup of the exact pre-upgrade
    /// bytes, or the bytes still on disk, no longer match its journal.
    #[expect(
        clippy::too_many_lines,
        reason = "the upgrade reads in transaction order; splitting it would separate the rollback decisions from the steps they depend on"
    )]
    pub async fn upgrade(&self, from: &Path, version: &str) -> Result<UpgradeReport, Error> {
        let _transaction = self.transaction().await?;
        upgrade_preflight(self.context)?;
        let config_path = self.context.config_path();
        let plan = upgrade_plan(self.store.load()?, version)?;
        let release = self.upgrade_source(&plan, from)?;
        let (gated, job_path) = self.adoption_gate(&plan, &release, version).await?;
        let (resume, rolled_back) = self.resolve_plan(plan).await?;
        // The file is read as an upgrade source, so a previous release's
        // schema-2 to schema-6 installation upgrades into this one
        // instead of being refused: the migration below carries its exact bytes
        // and schema.
        let source = load_config_for_upgrade(&config_path)?.ok_or(Error::NotInstalled {
            path: config_path.clone(),
        })?;
        if let Some(record) = &resume {
            // A resumed migration verifies its backup journal before any
            // further effect, so a moved, missing, or altered backup fails
            // the resume closed instead of at the rollback.
            verify_pending_migration(self.context, &self.store, record, &source)?;
        }
        let config = &source.config;
        check_upgrade_renderings(config, version, resume.is_some(), &config_path)?;
        let layout = config.layout().clone();
        check_layout_dirs(self.context, &layout)?;
        layout::claim_prefix(&layout, &config.namespace())?;
        let staged = layout::stage(&layout, &release, self.reported(version)).await?;
        let resumed = resume.is_some();
        let from_version = resume
            .as_ref()
            .and_then(|record| record.previous_version.clone())
            .unwrap_or_else(|| config.active_version().to_owned());
        if resume.is_none() && config.active_version() == version {
            let (removed_versions, kept_versions, gc_error) = self
                .refresh_active_version(staged, &layout, version)
                .await?;
            return Ok(UpgradeReport {
                warnings: unknown_keys_warning(config).into_iter().collect(),
                from_version,
                to_version: version.to_owned(),
                unchanged: true,
                resumed: false,
                removed_versions,
                kept_versions,
                gc_error,
                preflight: rolled_back
                    .as_ref()
                    .and_then(|report| report.preflight.clone()),
                accepted_runtime_loss: rolled_back
                    .as_ref()
                    .is_some_and(|report| report.accepted_runtime_loss),
                acceptance: AcceptanceGroups::default(),
                rolled_back,
                config_schema_migration: None,
                runtimes: None,
                integrations: Vec::new(),
            });
        }
        // A previous-schema source makes this a migration transaction: the record
        // journals the exact backup from the start, before any effect.
        let mut record = if let Some(record) = &resume {
            record.clone()
        } else {
            // A fresh upgrade swaps the daemon, so its early gate has read the
            // job `PATH` while the previous daemon is still loaded. Only the
            // launchd default of a loaded job is journaled: the other sources
            // can be read again later.
            let launchd_default_path = job_path.and_then(|job| job.loaded_default);
            let launchd_boot_identity = launchd_default_path
                .as_ref()
                .and_then(|_| journaled_boot_identity(self.context));
            let record = Record {
                schema_version: record::SCHEMA_VERSION,
                operation: Operation::Upgrade,
                version: version.to_owned(),
                prefix: layout.prefix().to_path_buf(),
                previous_version: Some(from_version.clone()),
                version_dir_preexisted: layout::version_exists(&layout, version)?,
                config_backup: config_backup_of(resume.as_ref(), &source),
                step: Step::Started,
                rolling_back: false,
                runtime_stop: false,
                runtime_backup: None,
                launchd_default_path,
                launchd_boot_identity,
                accepted_risks: None,
            };
            self.store.save(&record)?;
            record
        };
        let target = with_version(config, version)?;
        let mut late = LateGate::default();
        let result = self
            .run_steps(&mut record, &layout, staged, &target, &mut late)
            .await;
        // A refusal never touches the running daemon. Rolling back a record
        // at `Registering` would replace it, and the replacement may not have
        // happened yet, so that record is kept for a resume instead.
        let result = match result {
            Err(error) if late.refused && record.step >= Step::Registering => return Err(error),
            other => other,
        };
        self.settle(&record, result).await?;
        let (post, integrations) = self
            .reinstall_integrations(&layout, version, config, record.journal_default())
            .await;
        // The preflight right before the swap is the one that counts.
        let gated = late.gated.or(gated);
        let acceptance = gated
            .as_ref()
            .map(|gated| gated.acceptance.clone())
            .unwrap_or_default();
        // A ledger the gate ignored is reported even when the upgrade passes.
        let mut warnings = late.warnings;
        if let Some(gated) = &gated {
            warnings.extend(gated.warnings.iter().cloned());
        }
        warnings.extend(unknown_keys_warning(&target));
        let (removed_versions, kept_versions, gc_error) =
            self.collect_garbage_reported(&layout, version).await;
        Ok(UpgradeReport {
            warnings,
            from_version,
            to_version: version.to_owned(),
            unchanged: false,
            resumed,
            removed_versions,
            kept_versions,
            gc_error,
            accepted_runtime_loss: gated.as_ref().is_some_and(|gated| gated.accepted)
                || rolled_back
                    .as_ref()
                    .is_some_and(|report| report.accepted_runtime_loss),
            acceptance,
            preflight: gated.map(|Gated { report, .. }| report).or_else(|| {
                rolled_back
                    .as_ref()
                    .and_then(|report| report.preflight.clone())
            }),
            rolled_back,
            config_schema_migration: record
                .config_backup
                .as_ref()
                .map(|backup| backup.source_schema_version),
            runtimes: late.runtimes.or(post),
            integrations,
        })
    }

    /// Resolves the operator's release directory once for an upgrade, against
    /// the layout of the installation the upgrade changes. The same value is
    /// judged by the adoption gate and staged, so an installed version
    /// directory is verified before anything from it runs or is copied.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInstalled`] without an installation and the
    /// verification errors of [`layout::operator_release`].
    fn upgrade_source(&self, plan: &Plan, from: &Path) -> Result<VerifiedVersionDir, Error> {
        let layout = match plan {
            Plan::Fresh => {
                let config_path = self.context.config_path();
                load_config_for_upgrade(&config_path)?
                    .ok_or(Error::NotInstalled { path: config_path })?
                    .config
                    .layout()
                    .clone()
            }
            Plan::Resume(record) | Plan::RollBack(record) => install_layout(&record.prefix)?,
        };
        layout::operator_release(&layout, from)
    }

    /// Asks the new daemon's adoption preflight about the live sessions when
    /// `plan` swaps the daemon, and applies the operator's choice.
    ///
    /// Runs before any effect of the plan, a rollback included. The installed
    /// version is read leniently, so a previous-schema source is judged like any
    /// other.
    async fn adoption_gate(
        &self,
        plan: &Plan,
        release: &VerifiedVersionDir,
        version: &str,
    ) -> Result<(Option<Gated>, Option<JobPath>), Error> {
        // Only a fresh plan needs the installed version, which a rollback
        // would otherwise replace.
        let installed = match plan {
            Plan::Fresh => {
                load_config_for_upgrade(&self.context.config_path())?.map(|source| source.config)
            }
            Plan::Resume(_) | Plan::RollBack(_) => None,
        };
        if !swaps_daemon(
            plan,
            installed.as_ref().map(ServiceConfig::active_version),
            version,
        ) {
            return Ok((None, None));
        }
        let job = adoption_job_path(self.context, plan.record()).await?;
        let warnings = job_warnings(self.context, &job);
        let basis = match (&installed, plan) {
            (Some(config), _) => config.active_version().to_owned(),
            (None, Plan::Resume(record) | Plan::RollBack(record)) => self.gate_basis(record)?,
            (None, Plan::Fresh) => {
                unreachable!("a fresh plan loads the installed configuration")
            }
        };
        let acceptance = self.acceptance(&basis)?;
        let gated = preflight::gate_upgrade(
            &*self.preflight,
            self.context,
            release,
            self.reported(version),
            &job.value,
            &job.variables,
            preflight::ReleasePlan::Included,
            self.accept_runtime_loss,
            &acceptance,
        )
        .await
        .map_err(|error| error.with_warnings(&warnings))?;
        require_manager_variables(&job, &gated.report, self.accept_runtime_loss)?;
        Ok((Some(gated), Some(job)))
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
        let registered = self.resolve_uninstall_pending(options, &mut report).await?;
        let config_path = self.context.config_path();
        // The uninstall reads the installed configuration as an upgrade
        // source, so it removes a previous release's schema-2, schema-3,
        // schema-4, schema-5 or schema-6 installation with this version as well, without
        // a migration.
        let config = match (
            load_config_for_upgrade(&config_path)?.map(|source| source.config),
            &registered,
        ) {
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

        let reachable = match self
            .ensure_daemon(
                &definition,
                (&layout, &config.version_dir()),
                config.active_version(),
                &allowed_removal_versions(config.active_version(), registered.as_ref()),
                &mut report,
            )
            .await
        {
            // A supervised version outside the removal's set is a proven
            // property of the job, not an unreachable daemon: it refuses the
            // uninstall before any destructive call, whatever the sessions
            // and workers then report — even with none live.
            Err(error @ Error::SupervisedVersion { .. }) => return Err(error),
            reachable => reachable,
        };
        let sessions = match &reachable {
            Ok(()) => self
                .backend
                .control()
                .sessions()
                .await
                .map_err(|source| Error::Control {
                    operation: "session.list",
                    source: Box::new(source),
                })?,
            // An unreachable daemon proves no session of this installation
            // can be listed or stopped; without live sessions or workers the
            // removal below reaches the job (or its absence) directly.
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

    /// Handles a pending transaction before the session-checked removal.
    async fn resolve_uninstall_pending(
        &self,
        options: UninstallOptions,
        report: &mut UninstallReport,
    ) -> Result<Option<Record>, Error> {
        let Some(pending) = self.store.load()? else {
            return Ok(None);
        };
        // A transaction that may have registered a replacement is removed
        // through the session check. Restoring the previous daemon could
        // fail its reader gate, while uninstall has no runtime-loss flag.
        if pending.step >= Step::Registering {
            return Ok(Some(pending));
        }
        // A rollback removes an unregistered install's service.toml. Keep
        // its record during purge so a failed removal can be retried.
        let mut rolled_back = pending_report(&pending);
        let gated = if options.purge && pending.operation == Operation::Install {
            self.rollback(&pending).await?
        } else {
            self.rollback_pending(&pending).await?
        };
        if let Some(gated) = gated {
            rolled_back.accepted_runtime_loss = gated.accepted;
            rolled_back.warnings.clone_from(&gated.warnings);
            rolled_back.preflight = Some(gated.report);
        }
        report.rolled_back = Some(rolled_back);
        Ok(None)
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
            warnings: Vec::new(),
        };
        let Some(config) = config else {
            return Ok(report);
        };
        report.warnings.extend(unknown_keys_warning(config));
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
        late: &mut LateGate,
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

        self.run_migration_backup(record)?;
        self.checkpoint(Step::BackedUp)?;

        self.run_migration_daemon_stop(record, config, late).await?;
        self.checkpoint(Step::StopDaemon)?;

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

        // The old daemon keeps accepting sessions until it is replaced and no
        // facility pauses it, so the adoption preflight runs here, after
        // staging and as close to the replacement as the steps allow. A transaction
        // past registration has replaced it, and the new daemon's own adoption
        // decides. A migration already gated and stopped the daemon above;
        // nothing else can start between its stop and this point.
        let swap = record.operation == Operation::Upgrade
            && record.config_backup.is_none()
            && replaces_daemon(record.step);
        #[cfg(test)]
        if swap {
            if let Some(hook) = &self.before_swap {
                (hook.0)();
            }
        }
        // The job is resolved here, after the hook and after a migration's stop,
        // so the value the runtime step gets is the one the new daemon gets. A swap
        // or an apply that fails rolls back with the job unloaded, so the default
        // observed now is journaled first.
        if !record.runtime_stop {
            Box::pin(self.refresh_journaled_default(record, PathUse::Forward)).await?;
        }
        let job = job_path(self.context, config, record.journal_default()).await?;
        let warnings = job_warnings(self.context, &job);
        late.warnings.extend(warnings.iter().cloned());
        if swap {
            if let Some(dir) = config.daemon_executable().parent() {
                self.pass_swap_gate(record, layout, dir, &job, &warnings, late)
                    .await?;
            }
        }
        self.run_runtimes(record, layout, late, &job, &warnings)
            .await?;
        self.checkpoint(Step::Runtimes)?;
        if replaces_daemon(record.step) {
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
        self.write_ledger(record)?;
        self.store.clear()
    }

    /// The version whose sessions the gates of `record` judge: the one the
    /// upgrade replaces.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Record`] for a record that names no previous version.
    fn gate_basis(&self, record: &Record) -> Result<String, Error> {
        record
            .previous_version
            .clone()
            .ok_or_else(|| Error::Record {
                path: self.store.path(),
                detail: "an upgrade record names no previous version".to_owned(),
            })
    }

    /// The acceptances an earlier upgrade left for the installed `version`.
    ///
    /// Reading writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AcceptedRisks`] for a ledger that cannot be read safely.
    fn acceptance(&self, version: &str) -> Result<Acceptance, Error> {
        let namespace = self.context.namespace()?;
        let read = self.store.read_ledger()?;
        Acceptance::bind(
            read.as_deref(),
            &self.store.ledger_path(),
            namespace.as_str(),
            version,
        )
    }

    /// [`Self::acceptance`] for the gates of `record`.
    fn gate_acceptance(&self, record: &Record) -> Result<Acceptance, Error> {
        self.acceptance(&self.gate_basis(record)?)
    }

    /// Journals what the gate that just passed accepted, before the
    /// transaction advances past it.
    ///
    /// A later gate of the same run, or a resumed run that repeats this one,
    /// replaces the set, so consent always comes from the invocation that
    /// passed the gate.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the record cannot be saved.
    fn journal_gate(&self, record: &mut Record, gated: &mut Gated) -> Result<(), Error> {
        gated.accepted_set.recorded_at = now();
        record.accepted_risks = Some(gated.accepted_set.clone());
        self.store.save(record)
    }

    /// Replaces the accepted-risk ledger with the set the last gate journaled.
    ///
    /// It is the last effect before the record is cleared, so a crash before
    /// the clear repeats this idempotent replace and a rollback, which clears
    /// the record without it, never writes.
    fn write_ledger(&self, record: &Record) -> Result<(), Error> {
        let Some(set) = &record.accepted_risks else {
            return Ok(());
        };
        let namespace = self.context.namespace()?;
        self.store.write_ledger(&Ledger::of(
            namespace.as_str(),
            &record.version,
            record.previous_version.as_deref(),
            set,
        ))
    }

    /// Runs the adoption gate right before the daemon is replaced or stopped
    /// and journals what it accepted.
    ///
    /// A refusal leaves the running daemon untouched, which `late.refused`
    /// records.
    ///
    /// # Errors
    ///
    /// Returns the refusals of the gate and a filesystem error when the
    /// accepted set cannot be journaled.
    async fn pass_swap_gate(
        &self,
        record: &mut Record,
        layout: &InstallLayout,
        daemon_dir: &Path,
        job: &JobPath,
        warnings: &[String],
        late: &mut LateGate,
    ) -> Result<(), Error> {
        let acceptance = match self.gate_acceptance(record) {
            Ok(acceptance) => acceptance,
            Err(error) => {
                late.refused = true;
                return Err(error);
            }
        };
        let daemon_dir = match layout::verify_installed(layout, daemon_dir) {
            Ok(daemon_dir) => daemon_dir,
            Err(error) => {
                late.refused = true;
                return Err(error);
            }
        };
        match preflight::gate_upgrade(
            &*self.preflight,
            self.context,
            &daemon_dir,
            self.reported(&record.version),
            &job.value,
            &job.variables,
            preflight::ReleasePlan::Included,
            self.accept_runtime_loss,
            &acceptance,
        )
        .await
        {
            Ok(mut gated) => {
                if let Err(error) =
                    require_manager_variables(job, &gated.report, self.accept_runtime_loss)
                {
                    late.refused = true;
                    return Err(error);
                }
                self.journal_gate(record, &mut gated)?;
                late.gated = Some(gated);
                Ok(())
            }
            Err(error) => {
                late.refused = true;
                Err(error.with_warnings(warnings))
            }
        }
    }

    /// Verifies that the pre-upgrade bytes still on disk are the ones the
    /// migration journal digests, and returns them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigBackup`] instead of rewriting anything when the
    /// file does not match the journal any more.
    fn verify_migration_bytes(
        &self,
        record: &Record,
        backup: &ConfigBackup,
    ) -> Result<UpgradeSource, Error> {
        let source = load_config_for_upgrade(&self.context.config_path())?.ok_or_else(|| {
            Error::NotInstalled {
                path: self.context.config_path(),
            }
        })?;
        verify_migration_source(self.context, record, backup, &source)?;
        Ok(source)
    }

    /// Drops the daemon job after the adoption gate, before the rewrite.
    ///
    /// A missing job is tolerated: a resumed crash replay or an already
    /// uninstalled job only ever means the daemon is unreachable while its
    /// workers run on. The job is installed again when the new version is
    /// registered.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Supervisor`] when the service manager refuses.
    async fn stop_daemon_for_migration(&self) -> Result<(), Error> {
        match self.backend.daemon().uninstall().await {
            Ok(()) | Err(supervisor::Error::NotFound(_)) => Ok(()),
            Err(source) => Err(supervisor_error("stop daemon for config migration", source)),
        }
    }

    /// Saves the exact pre-upgrade bytes before the migration rewrites them.
    ///
    /// Runs under the `backed_up` step: the migration has not changed
    /// anything the previous daemon reads yet, so the file on disk still
    /// holds the exact bytes its digest journals — a mismatch is never
    /// rewritten, and a copy the resume cannot trust refuses the step.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigBackup`] for untrustworthy bytes and
    /// [`Error::Filesystem`] for the state directory.
    fn run_migration_backup(&self, record: &mut Record) -> Result<(), Error> {
        if record.step < Step::BackedUp {
            if let Some(backup) = &record.config_backup {
                let source = self.verify_migration_bytes(record, backup)?;
                self.store.write_config_backup(source.original_bytes())?;
            }
            self.advance(record, Step::BackedUp)?;
        }
        Ok(())
    }

    /// Records a durable stop intent, then stops the migration's old daemon.
    ///
    /// A migration stops the daemon before the rewrite: once `service.toml`
    /// is the new schema, a restart of the old daemon (its restart policy, a
    /// crash) would be refused by its strict reader and crash-loop. The
    /// adoption preflight runs first, so the still-live sessions are judged
    /// while the old daemon runs, exactly where the other flavors record
    /// their late gate before the swap. The other flavors mark the step
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns the preflight refusals without any effect, journaling
    /// failures, and [`Error::Supervisor`] when the service-manager stop
    /// refuses.
    async fn run_migration_daemon_stop(
        &self,
        record: &mut Record,
        config: &ServiceConfig,
        late: &mut LateGate,
    ) -> Result<(), Error> {
        if record.step > Step::StopDaemon {
            return Ok(());
        }
        if record.config_backup.is_none() {
            if record.step < Step::StopDaemon {
                self.advance(record, Step::StopDaemon)?;
            }
            return Ok(());
        }
        #[cfg(test)]
        if let Some(hook) = &self.before_swap {
            (hook.0)();
        }
        Box::pin(self.refresh_journaled_default(record, PathUse::Forward)).await?;
        let job = job_path(self.context, config, record.journal_default()).await?;
        let warnings = job_warnings(self.context, &job);
        late.warnings.extend(warnings.iter().cloned());
        if let Some(dir) = config.daemon_executable().parent() {
            self.pass_swap_gate(record, config.layout(), dir, &job, &warnings, late)
                .await?;
        }
        // Record the intent before the service-manager effect. A crash
        // between these calls replays the idempotent stop on resume, and
        // rollback knows that it may need to restart the old daemon.
        if record.step < Step::StopDaemon {
            self.advance(record, Step::StopDaemon)?;
        }
        self.stop_daemon_for_migration().await
    }

    /// Journals the launchd default the daemon job has right now, with the boot
    /// identity it was read at, before a step stops or replaces that job. Once the job is unloaded its default can no longer be
    /// read, so every later gate (a rollback after a failed swap or apply) judges
    /// the journaled one: this keeps it the value that was observed, not the one
    /// an earlier boot saw. A job whose default cannot be read leaves the journal.
    /// `usage` is the side of the transaction the call belongs to: a rollback is
    /// never refused for a `PATH` it cannot prove.
    ///
    /// # Errors
    ///
    /// Returns the store's error when the record cannot be saved.
    #[cfg(target_os = "macos")]
    async fn refresh_journaled_default(
        &self,
        record: &mut Record,
        usage: PathUse,
    ) -> Result<(), Error> {
        // Only an upgrade replaces a job that exists, and a job that records its
        // `PATH` never reads the launchd default.
        if record.operation != Operation::Upgrade {
            return Ok(());
        }
        if load_config_for_upgrade(&self.context.config_path())?
            .is_some_and(|source| !source.config.search_path().is_empty())
        {
            return Ok(());
        }
        let Ok(active) = active_launchd_job(self.context).await else {
            return Ok(());
        };
        let (Some(default), None) = (active.loaded_default, active.domain) else {
            return Ok(());
        };
        let boot = journaled_boot_identity(self.context);
        if boot.is_none() {
            // Without a boot identity the journal cannot prove the default after
            // the stop: refuse now, while the job still runs, unless the
            // persisted configuration would be taken for it then.
            journaled_or_persisted_path(
                self.context,
                JournalDefault {
                    path: &default,
                    boot_identity: None,
                },
                usage,
            )?;
        }
        if record.launchd_default_path.as_deref() == Some(default.as_str())
            && record.launchd_boot_identity == boot
        {
            return Ok(());
        }
        record.launchd_default_path = Some(default);
        record.launchd_boot_identity = boot;
        self.store.save(record)
    }

    /// Only launchd has a default to journal.
    #[cfg(not(target_os = "macos"))]
    #[expect(clippy::unused_self, reason = "the macOS form reads the launchd job")]
    fn refresh_journaled_default(
        &self,
        _record: &mut Record,
        _usage: PathUse,
    ) -> std::future::Ready<Result<(), Error>> {
        std::future::ready(Ok(()))
    }

    /// Plans and applies the release's official runtime packages and profile
    /// pins, stopping the previous daemon only when the plan changes something.
    ///
    /// The plan runs first and changes nothing; a refusal ends the step before
    /// any stop. An apply stops the daemon, journals the stop, backs up the
    /// state it may change (once, digests journaled before the apply), then
    /// applies and requires a finished report. A resumed step verifies the
    /// journaled backup instead of taking a new one, which would capture the
    /// state after an apply that may have started.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UpgradeRuntimesRefused`] for a refused plan,
    /// [`Error::UpgradeRuntimesFailed`] for an apply that does not finish, and
    /// [`Error::RuntimeBackup`] for a backup that cannot be taken or verified.
    async fn run_runtimes(
        &self,
        record: &mut Record,
        layout: &InstallLayout,
        late: &mut LateGate,
        job: &JobPath,
        warnings: &[String],
    ) -> Result<(), Error> {
        if record.step >= Step::Runtimes {
            return Ok(());
        }
        let search_path = job.value.as_str();
        let dir = layout
            .version_dir(&record.version)
            .ok_or_else(|| Error::Record {
                path: self.store.path(),
                detail: format!("version {} has no directory", record.version),
            })?;
        let dir = layout::verify_installed(layout, &dir)?;
        let plan = self
            .runtimes
            .run(self.context, &dir, true, search_path)
            .await?;
        runtimes::check_plan(&plan)?;
        if !runtimes::needs_apply(&plan) {
            late.runtimes = Some(plan);
            return self.advance(record, Step::Runtimes);
        }
        // The stop intent is journaled first, so a rollback that finds it
        // restarts the previous daemon even if the stop did not return.
        if !record.runtime_stop {
            record.runtime_stop = true;
            self.store.save(record)?;
        }
        self.stop_daemon_for_migration().await?;
        if let Some(backup) = &record.runtime_backup {
            runtimes::verify_backup(backup, &self.store)?;
        } else {
            let captured = runtimes::capture(self.context)?;
            let files: Vec<Vec<u8>> = captured.iter().map(|file| file.bytes.clone()).collect();
            self.store.write_runtime_backup(&files)?;
            record.runtime_backup = Some(runtimes::journal(&captured));
            self.store.save(record)?;
        }
        let applied = self
            .runtimes
            .run(self.context, &dir, false, search_path)
            .await?;
        runtimes::check_applied(dir.path(), &applied)?;
        late.runtimes = Some(applied);
        if record.operation == Operation::Upgrade {
            // The verdict taken before the apply judged the packages that were
            // installed then. The apply may select another package, so the new
            // release judges the live sessions again, with the same decision as
            // the late gate. A refusal or a preflight that cannot run fails this
            // step, which rolls the apply back.
            let acceptance = self.gate_acceptance(record)?;
            let mut gated = preflight::gate_upgrade(
                &*self.preflight,
                self.context,
                &dir,
                self.reported(&record.version),
                search_path,
                &job.variables,
                // The apply has run, so the installed packages are the plan's result.
                preflight::ReleasePlan::Omitted,
                self.accept_runtime_loss,
                &acceptance,
            )
            .await
            .map_err(|error| error.with_warnings(warnings))?;
            require_manager_variables(job, &gated.report, self.accept_runtime_loss)?;
            self.journal_gate(record, &mut gated)?;
            late.gated = Some(gated);
        }
        self.advance(record, Step::Runtimes)
    }

    /// Reads the integration hooks the new daemon's release lists and
    /// reinstalls them, with the daemon job's `PATH` for both the plan and the
    /// Hermes executable. Runs after the transaction, so a failure here is only
    /// reported and never rolls the upgrade back.
    async fn reinstall_integrations(
        &self,
        layout: &InstallLayout,
        version: &str,
        config: &ServiceConfig,
        journal_default: Option<JournalDefault<'_>>,
    ) -> (Option<EnsureReport>, Vec<IntegrationReinstall>) {
        let plan = match layout.version_dir(version) {
            Some(dir) => match layout::verify_installed(layout, &dir) {
                Ok(dir) => match job_search_path(self.context, config, journal_default).await {
                    Ok(job) => self
                        .runtimes
                        .run(self.context, &dir, true, &job.value)
                        .await
                        .map(|plan| (plan, job)),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            None => Err(Error::Record {
                path: self.store.path(),
                detail: format!("version {version} has no directory"),
            }),
        };
        match plan {
            Ok((plan, job)) => {
                let env = IntegrationEnv {
                    paths: Paths::from_base(self.context.paths()),
                    home: self.context.home().map(Path::to_path_buf),
                    state_dir: self.context.paths().state_dir.clone(),
                };
                let results = self
                    .integrations
                    .reinstall(&env, &plan.integrations, &job.value)
                    .await;
                (Some(plan), results)
            }
            Err(error) => (
                None,
                vec![IntegrationReinstall {
                    integration: "runtime plan".to_owned(),
                    status: "failed",
                    detail: Some(error.to_string()),
                }],
            ),
        }
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
        // The runtime step's daemon may still run, so the transaction is kept
        // as it is: rolling back would restore state under a live writer.
        if matches!(error, Error::UpgradeRuntimesUnconfirmed { .. }) {
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
            Ok(_gated) => {
                self.store.clear()?;
                Err(error)
            }
            // The restore is kept, not failed over: the typed error reaches the
            // operator, and the next command resumes the rollback.
            Err(rollback @ Error::UpgradeRuntimesUnconfirmed { .. }) => Err(rollback),
            Err(rollback) => Err(Error::RollbackFailed {
                original: Box::new(error),
                rollback: Box::new(rollback),
            }),
        }
    }

    /// Rolls back an interrupted transaction found at startup.
    async fn rollback_pending(&self, record: &Record) -> Result<Option<Gated>, Error> {
        let gated = self.rollback(record).await?;
        self.store.clear()?;
        Ok(gated)
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
    ///
    /// A migration rollback validates the backup and the previous reader
    /// before stopping any registered new daemon, judges the store again
    /// after that stop, restores exact schema-2 bytes, and only then starts
    /// the previous daemon. An invalid backup fails before any effect.
    async fn rollback(&self, record: &Record) -> Result<Option<Gated>, Error> {
        if record.operation == Operation::Upgrade && rollback_stops_daemon(record) {
            // The stop unloads the job: journal the default it has now first.
            let mut journaled = record.clone();
            Box::pin(self.refresh_journaled_default(&mut journaled, PathUse::Rollback)).await?;
            return Box::pin(self.rollback_journaled(&journaled)).await;
        }
        Box::pin(self.rollback_journaled(record)).await
    }

    async fn rollback_journaled(&self, record: &Record) -> Result<Option<Gated>, Error> {
        let layout = install_layout(&record.prefix)?;
        let config_path = self.context.config_path();
        let mut gated = None;
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
                self.restore_runtime_state(record, &layout)?;
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
                if record.config_backup.is_some() {
                    // Validate the exact backup before stopping a healthy new
                    // daemon or changing the transaction record.
                    self.rollback_source(record, previous, &config_path)?;
                }
                let restarts_previous = rollback_restarts_previous(record);
                // A new daemon may be running and the store is still the one it
                // left, so judge before stopping it. A runtime backup is restored
                // before the daemon stops; the gates after the stop judge that
                // restored state, not the applied one.
                let new_daemon_may_run = record.step >= Step::Registering
                    || (record.config_backup.is_some() && record.step >= Step::StopDaemon);
                if restarts_previous && new_daemon_may_run && record.runtime_backup.is_none() {
                    gated = Some(self.rollback_gate(&layout, previous, record).await?);
                }
                let stops_daemon = rollback_stops_daemon(record);
                if restarts_previous && stops_daemon && record.runtime_backup.is_some() {
                    // The gate waits for the restore, but the job's `PATH` must be
                    // resolvable before the daemon is stopped: the gate after the
                    // stop would otherwise refuse with the daemon down.
                    journal_job_path(self.context, record).await?;
                }
                self.mark_rolling_back(record)?;
                if stops_daemon {
                    // A journaled migration stop may not have happened yet;
                    // a later swap may have registered the new daemon. Stop
                    // either job before restoring previous-schema bytes so neither
                    // can restart against a file it cannot read.
                    match self.backend.daemon().uninstall().await {
                        Ok(()) | Err(supervisor::Error::NotFound(_)) => {}
                        Err(source) => {
                            return Err(supervisor_error("stop daemon for rollback", source));
                        }
                    }
                }
                #[cfg(feature = "test-util")]
                if let Some(hook) = &self.after_rollback_stop {
                    (hook.0)();
                }
                self.restore_runtime_state(record, &layout)?;
                if restarts_previous {
                    // The new daemon may have written the store while it was
                    // stopping. Judge the previous reader again, on the restored
                    // state, before the config restore and daemon start.
                    gated = Some(self.rollback_gate(&layout, previous, record).await?);
                }
                let restored = self.restore_rollback_config(record, previous, &config_path)?;
                if restarts_previous {
                    // The swap stopped the old daemon by replacing its job;
                    // the migration stopped it on purpose. Its own file is
                    // back in place now, so it is installed again from it.
                    let definition = daemon_definition(self.context, &restored)?;
                    self.backend
                        .daemon()
                        .install(&definition)
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
            return Ok(gated);
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
        Ok(gated)
    }

    /// Puts back the state files the runtime step backed up, if it backed any up.
    ///
    /// Runs after the daemon that may have written them is stopped and before
    /// the previous daemon starts. The restore holds the data authority lock, so
    /// a running daemon that still owns the data keeps the transaction instead.
    fn restore_runtime_state(&self, record: &Record, layout: &InstallLayout) -> Result<(), Error> {
        match &record.runtime_backup {
            Some(backup) => {
                let daemon =
                    layout
                        .daemon_executable(&record.version)
                        .ok_or_else(|| Error::Record {
                            path: self.store.path(),
                            detail: format!("version {} has no daemon executable", record.version),
                        })?;
                runtimes::restore(self.context, &daemon, &self.store, backup)
            }
            None => Ok(()),
        }
    }

    /// Judges a rollback using the previous version's daemon reader.
    async fn rollback_gate(
        &self,
        layout: &InstallLayout,
        previous: &str,
        record: &Record,
    ) -> Result<Gated, Error> {
        let daemon_dir =
            layout
                .version_dir(previous)
                .ok_or_else(|| Error::RollbackPreflightFailed {
                    version: previous.to_owned(),
                    detail: "previous version has no valid installation directory".to_owned(),
                })?;
        let daemon_dir = layout::verify_installed(layout, &daemon_dir)?;
        let job = journal_job_path(self.context, record).await?;
        let warnings = job_warnings(self.context, &job);
        let mut gated = rollback::gate(
            rollback::Inputs {
                preflight: &*self.preflight,
                context: self.context,
                backend: Some(self.backend),
                layout,
                inspector: &*self.inspector,
            },
            &daemon_dir,
            previous,
            self.reported(previous),
            &job.value,
            &job.variables,
            self.accept_runtime_loss,
        )
        .await
        .map_err(|error| error.with_warnings(&warnings))?;
        require_manager_variables(&job, &gated.report, self.accept_runtime_loss)?;
        gated.warnings = warnings;
        Ok(gated)
    }

    /// Restores the `service.toml` an upgrade record rolls back to.
    ///
    /// A migration record restores the exact backed-up pre-upgrade bytes
    /// after verifying them against the record's digest, so the previous
    /// daemon starts against the file it wrote itself. Every other record
    /// rewrites the current file with the previous version, which the
    /// current-schema reader of the running daemon understands.
    ///
    /// # Errors
    ///
    /// Returns the configuration read or write errors, and
    /// [`Error::ConfigBackup`] for a missing, altered, or schema-mismatched
    /// backup, without writing anything.
    fn restore_rollback_config(
        &self,
        record: &Record,
        previous: &str,
        config_path: &Path,
    ) -> Result<ServiceConfig, Error> {
        if record.config_backup.is_some() {
            let source = self.rollback_source(record, previous, config_path)?;
            if record.step >= Step::StopDaemon {
                source.write_original(config_path)?;
            }
            return Ok(source.config);
        }
        let current = load_config(config_path)?.ok_or(Error::NotInstalled {
            path: config_path.to_path_buf(),
        })?;
        let restored = with_version(&current, previous)?;
        // A file that never left the previous version is already restored;
        // rewriting it could only fail on a rendering above the load bound.
        if current.active_version() != previous {
            restored.write(config_path)?;
        }
        Ok(restored)
    }

    /// Reads and verifies the exact config a migration rollback would restore.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigBackup`] or [`Error::Record`] before any write
    /// when the file, backup, schema, or previous version disagrees.
    fn rollback_source(
        &self,
        record: &Record,
        previous: &str,
        config_path: &Path,
    ) -> Result<UpgradeSource, Error> {
        let backup = record.config_backup.as_ref().ok_or_else(|| Error::Record {
            path: self.store.path(),
            detail: "migration rollback has no config backup".to_owned(),
        })?;
        let current = ServiceConfig::load_upgrade_source(config_path)?;
        verify_pending_migration(self.context, &self.store, record, &current)?;
        let source = if record.step <= Step::StopDaemon {
            if record.step == Step::StopDaemon && digest_mismatch(current.original_bytes(), backup)
            {
                let bytes = self
                    .store
                    .read_config_backup()?
                    .ok_or_else(|| backup_error(self.store.config_backup_path(), "is missing"))?;
                ServiceConfig::load_upgrade_source_bytes(&self.store.config_backup_path(), &bytes)?
            } else {
                current
            }
        } else {
            let bytes = self
                .store
                .read_config_backup()?
                .ok_or_else(|| backup_error(self.store.config_backup_path(), "is missing"))?;
            ServiceConfig::load_upgrade_source_bytes(config_path, &bytes)?
        };
        if digest_mismatch(source.original_bytes(), backup) {
            let path = if record.step < Step::Config {
                config_path.to_path_buf()
            } else {
                self.store.config_backup_path()
            };
            return Err(backup_error(
                path,
                "does not match the digest journaled in the transaction record",
            ));
        }
        if source.source_schema_version != backup.source_schema_version {
            return Err(backup_error(
                self.store.config_backup_path(),
                format!(
                    "is schema {}, but the transaction record recorded schema {}",
                    source.source_schema_version, backup.source_schema_version
                ),
            ));
        }
        if source.config.active_version() != previous {
            return Err(Error::Record {
                path: self.store.path(),
                detail: format!(
                    "the transaction record names previous version {previous}, \
                     but its backup names {}",
                    source.config.active_version()
                ),
            });
        }
        Ok(source)
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
        self.unit_verifier
            .verify(&config.namespace(), definition)
            .await
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
            (Operation::Upgrade, _) => match daemon.replace(definition).await {
                // A migration stopped the daemon job before it rewrote
                // `service.toml`, so there is nothing to replace; the new
                // definition is installed instead. The fallback also covers a
                // daemon job that disappeared on its own during the
                // transaction.
                Err(supervisor::Error::NotFound(_)) => daemon
                    .install(definition)
                    .await
                    .map_err(|source| supervisor_error("install daemon", source)),
                result => result.map_err(|source| supervisor_error("replace daemon", source)),
            },
        }
    }

    /// Waits until the supervised daemon answers `daemon.health` as `version`.
    ///
    /// The answer counts only when the process serving the socket is the
    /// daemon job's running main process. Another process of the same build,
    /// such as a manually started daemon, can hold the socket while the
    /// supervised job crash-loops, and must never make a transaction ready.
    async fn wait_ready(&self, version: &str) -> Result<(), Error> {
        self.wait_served(&Serving::Strict(version), version).await
    }

    /// Waits until the supervised daemon answers `daemon.health`.
    ///
    /// [`Serving::Strict`] compares the answer against [`Engine::reported`]
    /// of that one version; [`Serving::Allowed`] accepts any version in the
    /// set, applied through [`Engine::reported`] as well. The answer counts
    /// only when the process serving the socket is the daemon job's running
    /// main process, and a job that serves a version outside its expectation
    /// is refused immediately instead of being waited for.
    async fn wait_served(&self, serving: &Serving<'_>, version: &str) -> Result<(), Error> {
        let deadline = Instant::now() + self.ready_timeout;
        loop {
            let mut last = match self.backend.control().health().await {
                Ok(health) if self.serving_matches(&health.result.daemon_version, serving) => {
                    match self.served_by_job(health.pid).await {
                        Ok(()) => return Ok(()),
                        Err(mismatch) => Some(mismatch),
                    }
                }
                Ok(health) => match serving {
                    Serving::Strict(_) => Some(format!(
                        "daemon reports version {}",
                        health.result.daemon_version
                    )),
                    Serving::Allowed(allowed) => match self.served_by_job(health.pid).await {
                        Ok(()) => {
                            return Err(Error::SupervisedVersion {
                                served: health.result.daemon_version.clone(),
                                accepted: allowed.to_vec(),
                            });
                        }
                        Err(mismatch) => Some(mismatch),
                    },
                },
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
    ///
    /// A daemon that already runs is left alone and answered by the version
    /// it actually serves, which a removal may reach only within `allowed`
    /// ([`allowed_removal_versions`]; see [`Error::SupervisedVersion`]).
    /// A daemon started here is the configured one and must report `version`
    /// exactly. Starting it first verifies the active version directory
    /// `active`, so an untrusted directory is never executed and the daemon
    /// counts as unreachable.
    async fn ensure_daemon(
        &self,
        definition: &JobDefinition,
        active: (&InstallLayout, &Path),
        version: &str,
        allowed: &[String],
        report: &mut UninstallReport,
    ) -> Result<(), Error> {
        let daemon = self.backend.daemon();
        let running = match daemon.inspect().await {
            Ok(observation) if observation.state == ServiceState::Running => true,
            Ok(_stopped) => {
                layout::verify_installed(active.0, active.1)?;
                daemon
                    .replace(definition)
                    .await
                    .map_err(|source| supervisor_error("start daemon", source))?;
                report.started_daemon = true;
                false
            }
            Err(supervisor::Error::NotFound(_)) => {
                layout::verify_installed(active.0, active.1)?;
                daemon
                    .install(definition)
                    .await
                    .map_err(|source| supervisor_error("start daemon", source))?;
                report.started_daemon = true;
                false
            }
            Err(source) => return Err(supervisor_error("inspect daemon", source)),
        };
        if running {
            self.wait_served(&Serving::Allowed(allowed), version).await
        } else {
            self.wait_ready(version).await
        }
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
        self.discover_strict_settled()
            .await
            .map_err(|source| supervisor_error("discover workers", source))
    }

    /// Strict discovery that retries a raced job until the stop timeout.
    ///
    /// A job being unloaded or removed while it is inspected is a transient
    /// race; every other failure is returned at once.
    async fn discover_strict_settled(&self) -> Result<Vec<ServiceObservation>, supervisor::Error> {
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            match self.backend.workers().discover_strict().await {
                Err(supervisor::Error::Race { .. }) if Instant::now() < deadline => {
                    tokio::time::sleep(settings::POLL_INTERVAL).await;
                }
                result => return result,
            }
        }
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
                for backup in schema_backups(&data, name)? {
                    record::remove_file(&data, &backup)?;
                    report.removed.push(paths.data_dir.join(backup));
                }
            }
            purge_store_artifacts(&data, report)?;
            for name in PURGED_DATA_DIRS {
                if layout::remove_tree(&data, name)? {
                    report.removed.push(paths.data_dir.join(name));
                }
            }
            report.kept_purge_entries.extend(
                data.entry_names()
                    .map_err(|source| super::error::fs_error("list retained data entries", source))?
                    .into_iter()
                    .map(|name| paths.data_dir.join(name)),
            );
        }
        if let Some(state) = layout::open_existing_owner_dir(&paths.state_dir)? {
            for name in PURGED_STATE_DIRS {
                if layout::remove_tree(&state, name)? {
                    report.removed.push(paths.state_dir.join(name));
                }
            }
        }
        report.kept_purge_entries.sort();
        Ok(())
    }

    /// Re-installs the CLI of an already active version and collects its
    /// garbage, for an upgrade that restarts nothing.
    ///
    /// The staged binaries are only proven to match the existing version
    /// directory; they were meant for this same version and replace nothing.
    ///
    /// # Errors
    ///
    /// Returns the staging-trust and CLI-install errors before any garbage
    /// is collected.
    async fn refresh_active_version(
        &self,
        staged: layout::Staged,
        layout: &InstallLayout,
        version: &str,
    ) -> Result<(Vec<String>, Vec<KeptVersion>, Option<String>), Error> {
        layout::verify_existing(layout, &staged, version)?;
        layout::discard(layout, &staged)?;
        layout::install_cli(layout, version)?;
        Ok(self.collect_garbage_reported(layout, version).await)
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
        let discovered = self.discover_strict_settled().await;
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

/// The reported version a supervised readiness check accepts.
enum Serving<'a> {
    /// Only this version makes the daemon ready: a transaction observes its
    /// one replacement, so readiness is the proof it happened.
    Strict(&'a str),
    /// Every named version is accepted: the supervised job may have been
    /// replaced before the job registration landed, and serving through that
    /// daemon is the removal's job.
    Allowed(&'a [String]),
}

/// The versions a removal may serve through: the configured one, plus — for
/// a pending upgrade at or after `registering` — the version whose daemon the
/// interrupted transaction may have left running while `service.toml` already
/// names the new one. Any other supervised version is refused before the
/// removal touches a session.
fn allowed_removal_versions(active: &str, pending: Option<&Record>) -> Vec<String> {
    let mut allowed = vec![active.to_owned()];
    if let Some(pending) = pending {
        if pending.operation == Operation::Upgrade && pending.step >= Step::Registering {
            if let Some(previous) = pending.previous_version.as_deref().filter(|p| *p != active) {
                allowed.push(previous.to_owned());
            }
        }
    }
    allowed
}

/// Whether rollback would start the previous daemon after this recorded step.
pub(crate) fn rollback_restarts_previous(record: &Record) -> bool {
    record.operation == Operation::Upgrade
        && (record.step >= Step::Registering
            || record.runtime_stop
            || (record.config_backup.is_some() && record.step >= Step::StopDaemon))
}

/// Returns whether a session may still own a live PTY.
fn is_live(session: &SessionInfo) -> bool {
    session.may_own_runtime()
}

/// Returns whether a worker job may still own a live PTY.
///
/// A process proves it directly, and `Starting`, `Running`, and `Stopping`
/// prove it through the service manager. `Unknown` also counts: launchd
/// reports a loaded job without a matching process as `Unknown`, and such a
/// job may still be waiting to spawn. Only `Stopped` and `Failed` prove the
/// end without further evidence; a caller that needs more proof reads the
/// worker journal.
pub(super) fn job_alive(observation: &ServiceObservation) -> bool {
    observation.process.is_some()
        || matches!(
            observation.state,
            ServiceState::Starting
                | ServiceState::Running
                | ServiceState::Stopping
                | ServiceState::Unknown
        )
}

/// The current time, RFC 3339.
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("a UTC timestamp always formats")
}

/// The report of a pending transaction. Its warnings come from the rollback
/// that judges it, which knows what it resolved.
pub(crate) fn pending_report(record: &Record) -> PendingReport {
    PendingReport {
        operation: record.operation.as_str(),
        version: record.version.clone(),
        step: record.step.as_str(),
        preflight: None,
        accepted_runtime_loss: false,
        warnings: Vec::new(),
    }
}

/// Every warning a judgement made with `job` owes the operator. Each caller
/// that judges with the job's `PATH` or variables reports them through this, so
/// none can forget one.
pub(crate) fn job_warnings(context: &Context, job: &JobPath) -> Vec<String> {
    let mut warnings = persisted_path_warnings(context, job);
    warnings.extend(manager_warnings(job));
    warnings.extend(drop_in_path_warnings(job));
    warnings
}

/// The warning of a recorded `PATH` that a drop-in of the daemon unit replaces:
/// the job runs with the drop-in's value, and the judgement uses it.
fn drop_in_path_warnings(job: &JobPath) -> Vec<String> {
    let Some(drop_in) = &job.path_drop_in else {
        return Vec::new();
    };
    let warning = format!(
        "the drop-in {drop_in} of the daemon unit sets PATH and replaces the PATH service.toml records, so the daemon job runs with the drop-in's PATH and this judgement uses it. Remove the PATH assignment from the drop-in to run with the recorded one"
    );
    tracing::warn!(drop_in = %drop_in, "{warning}");
    vec![warning]
}

/// Refuses a transaction gate's judgement that ran without the service
/// manager's variables and judged a stopped session, unless runtime loss is
/// accepted. Only a stopped session's native reference check reads a store
/// location those variables can move; judged at the default location, it may
/// pass a session that loses its recovery, or call a lost one unchanged. A
/// judgement without stopped sessions does not depend on the variables.
pub(crate) fn require_manager_variables(
    job: &JobPath,
    report: &PreflightReport,
    accept_runtime_loss: bool,
) -> Result<(), Error> {
    match &job.manager_unreachable {
        Some(detail) if !accept_runtime_loss && !report.stopped_sessions.is_empty() => {
            Err(Error::ManagerVariablesUnread {
                detail: detail.clone(),
            })
        }
        _ => Ok(()),
    }
}

/// The warning of a launchd `PATH` that was persisted: the job is not loaded and
/// the transaction journal holds no launchd default that is known to apply (none,
/// or one from before a reboot). Only a transaction can have one.
#[cfg(target_os = "macos")]
fn persisted_path_warnings(context: &Context, job: &JobPath) -> Vec<String> {
    if job.source != PathSource::Persisted {
        return Vec::new();
    }
    let journal = context.paths().state_dir.join(record::FILE_NAME);
    let warning = format!(
        "the launchd job is not loaded and the transaction journal {} holds no launchd default PATH that is known to apply, so the persisted launchd configuration was used (launchd's standard PATH when it cannot be read)",
        journal.display()
    );
    tracing::warn!(journal = %journal.display(), "{warning}");
    vec![warning]
}

/// No launchd job runs on other hosts, so no `PATH` is persisted there.
#[cfg(not(target_os = "macos"))]
fn persisted_path_warnings(_context: &Context, _job: &JobPath) -> Vec<String> {
    Vec::new()
}

/// The `PATH` a rollback of `record` judges with: the one the installed
/// configuration gives the job, with the launchd default the upgrade journaled
/// while the previous job was loaded standing in when the job is not loaded.
pub(crate) async fn journal_job_path(context: &Context, record: &Record) -> Result<JobPath, Error> {
    let source = load_config_for_upgrade(&context.config_path())?.ok_or(Error::NotInstalled {
        path: context.config_path(),
    })?;
    job_path_for(
        context,
        &source.config,
        record.journal_default(),
        PathUse::Rollback,
    )
    .await
}

/// Whether rolling back `record` stops the daemon job before restoring.
pub(crate) fn rollback_stops_daemon(record: &Record) -> bool {
    record.step >= Step::Registering
        || record.runtime_stop
        || (record.config_backup.is_some() && record.step >= Step::StopDaemon)
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
///
/// Rolling back an interrupted install leaves nothing installed, so install
/// goes on to write a fresh file. Rolling back an interrupted upgrade restores
/// the existing `service.toml`, so nothing is written and the rollback must
/// need neither discovery nor a valid discovery environment.
pub(crate) fn plan_discovers(plan: &Plan) -> bool {
    match plan {
        Plan::Fresh => true,
        Plan::RollBack(record) => record.operation == Operation::Install,
        Plan::Resume(record) => record.step < Step::Config,
    }
}

/// Whether a transaction recorded at `step` still has the daemon to replace.
///
/// `run_steps` replaces the daemon exactly when this holds, and the adoption
/// preflight gates exactly then, so the two cannot drift apart. `Registering`
/// is recorded before the service-manager call, so an interrupted transaction
/// at that step may not have replaced it yet.
fn replaces_daemon(step: Step) -> bool {
    step < Step::Registered
}

/// Whether upgrading to `version` under `plan` swaps the daemon, so the
/// adoption preflight judges the live sessions first.
///
/// A resumed upgrade that has not registered the daemon still has to replace
/// it, a later one already has, a rerun of the active version
/// swaps nothing, and a rolled-back one starts over.
pub(crate) fn swaps_daemon(plan: &Plan, installed: Option<&str>, version: &str) -> bool {
    match plan {
        Plan::Resume(record) => replaces_daemon(record.step),
        // The rollback restores the previous version, so asking for it again
        // swaps in nothing new.
        Plan::RollBack(record) => record.previous_version.as_deref() != Some(version),
        Plan::Fresh => installed.is_some_and(|active| active != version),
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
pub(crate) fn install_layout(prefix: &Path) -> Result<InstallLayout, Error> {
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

/// Loads `service.toml` as an upgrade source, or `None` when it does not
/// exist.
///
/// The current schema and the previous schemas 2, 3, 4 and 5 are accepted; a
/// schema the binary neither writes nor upgrades from is refused with its
/// regular error, which names `pohunek service uninstall` and `install`.
fn load_config_for_upgrade(path: &Path) -> Result<Option<UpgradeSource>, Error> {
    if file_exists(path)? {
        Ok(Some(ServiceConfig::load_upgrade_source(path)?))
    } else {
        Ok(None)
    }
}

/// The `PATH` the installed daemon job runs with, outside a transaction.
///
/// # Errors
///
/// See [`job_path`].
async fn installed_job_path(context: &Context) -> Result<JobPath, Error> {
    installed_job_path_in(context, None, PathUse::Forward).await
}

/// The `PATH` the adoption preflight of an upgrade that swaps the daemon judges
/// with, for the engine's gate and `service check` alike.
///
/// `pending` is the record the upgrade resumes or rolls back: it judges with the
/// launchd default that transaction journaled, as the gates after it do, and
/// without one the installed job's `PATH`.
///
/// # Errors
///
/// See [`job_path`].
pub(crate) async fn adoption_job_path(
    context: &Context,
    pending: Option<&Record>,
) -> Result<JobPath, Error> {
    match pending {
        None => installed_job_path(context).await,
        Some(record) => {
            installed_job_path_in(context, record.journal_default(), PathUse::Forward).await
        }
    }
}

/// [`installed_job_path`] inside a transaction: `journal_default` is the launchd
/// default it journaled, and `usage` the side of it the judgement serves.
///
/// # Errors
///
/// See [`job_path`].
async fn installed_job_path_in(
    context: &Context,
    journal_default: Option<JournalDefault<'_>>,
    usage: PathUse,
) -> Result<JobPath, Error> {
    let source = load_config_for_upgrade(&context.config_path())?.ok_or(Error::NotInstalled {
        path: context.config_path(),
    })?;
    job_path_for(context, &source.config, journal_default, usage).await
}

/// The `PATH` the installed daemon job runs with, as [`job_search_path`] gives
/// it: no manager variables are selected.
///
/// # Errors
///
/// See [`job_path`].
pub(crate) async fn installed_job_search_path(context: &Context) -> Result<JobPath, Error> {
    let source = load_config_for_upgrade(&context.config_path())?.ok_or(Error::NotInstalled {
        path: context.config_path(),
    })?;
    job_search_path(context, &source.config, None).await
}

/// The launchd default a transaction journaled, with the boot it was read in.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    not(target_os = "macos"),
    expect(
        dead_code,
        reason = "only the launchd path reads the journaled default"
    )
)]
pub(crate) struct JournalDefault<'a> {
    /// The loaded job's `default environment` `PATH`.
    pub(crate) path: &'a str,
    /// The boot identity read with it, when it could be.
    pub(crate) boot_identity: Option<&'a str>,
}

impl Record {
    /// The launchd default this record journaled, with its boot identity.
    pub(crate) fn journal_default(&self) -> Option<JournalDefault<'_>> {
        self.launchd_default_path
            .as_deref()
            .map(|path| JournalDefault {
                path,
                boot_identity: self.launchd_boot_identity.as_deref(),
            })
    }
}

/// Where the `PATH` of a job without its own came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathSource {
    /// The `PATH` `service.toml` records for the job.
    Recorded,
    /// A drop-in of the daemon unit (Linux), which replaces the recorded `PATH`.
    DropIn,
    /// The systemd user manager's `Environment`, read now (Linux).
    UserManager,
    /// The launchd domain's `environment`, set by `launchctl setenv`.
    #[cfg(target_os = "macos")]
    Domain,
    /// The `default environment` of the loaded job (launchd).
    #[cfg(target_os = "macos")]
    LoadedDefault,
    /// The launchd default the upgrade journaled while the job was loaded.
    #[cfg(target_os = "macos")]
    Journal,
    /// The persisted launchd configuration, or launchd's standard `PATH`, as a
    /// stand-in: a change of it may be pending, so it may not be the active one.
    #[cfg(target_os = "macos")]
    Persisted,
    /// The persisted launchd configuration, or launchd's standard `PATH`, which
    /// was last changed before the host booted and is therefore the one launchd
    /// applies to a job that is not loaded.
    #[cfg(target_os = "macos")]
    PersistedActive,
}

/// Which side of a transaction a daemon job's `PATH` is judged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathUse {
    /// A gate before or while the transaction moves forward (preflight, runtime
    /// step, `service check`, setup): it refuses a `PATH` that cannot be told.
    Forward,
    /// A rollback or its gate: it goes on with the best evidence, because
    /// refusing would strand the host between two versions.
    Rollback,
}

/// The `PATH` a daemon job runs with, byte for byte, and where it came from.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct JobPath {
    /// The value the job's processes get.
    pub(crate) value: String,
    /// Where the value came from.
    pub(crate) source: PathSource,
    /// The drop-in of the daemon unit that replaces the `PATH` `service.toml`
    /// records, when one does (Linux).
    pub(crate) path_drop_in: Option<String>,
    /// The launchd `default environment` `PATH` of the loaded job, whatever
    /// `value` is. An upgrade journals it; `None` when the job is not loaded and
    /// always on Linux.
    pub(crate) loaded_default: Option<String>,
    /// The allowlisted variables the service manager gives the job, `PATH`
    /// excluded. A preflight judges with them, so they stay out of the logs and
    /// errors: only their names may appear there.
    pub(crate) variables: BTreeMap<String, String>,
    /// Why the service manager's environment was not read: on Linux a job that
    /// records its `PATH` with no reachable manager, on macOS any job when launchd
    /// has no user domain. Its variables are then not applied.
    pub(crate) manager_unreachable: Option<String>,
}

// The variables are printed by name only: their values may be secrets.
impl std::fmt::Debug for JobPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobPath")
            .field("value", &self.value)
            .field("source", &self.source)
            .field("path_drop_in", &self.path_drop_in)
            .field("loaded_default", &self.loaded_default)
            .field("variables", &self.variables.keys().collect::<Vec<_>>())
            .field("manager_unreachable", &self.manager_unreachable)
            .finish_non_exhaustive()
    }
}

/// What makes the service manager's environment readable on this platform.
#[cfg(target_os = "macos")]
const MANAGER_REMEDY: &str =
    "Log the user in to a GUI session, so launchd has its gui/<uid> domain";
/// What makes the service manager's environment readable on this platform.
#[cfg(not(target_os = "macos"))]
const MANAGER_REMEDY: &str = "Start the user's service manager or make its bus reachable";

/// The warning a judgement carries when the service manager's variables were
/// not read ([`JobPath::manager_unreachable`]). The verdicts may err in either direction, so the text
/// says which way and what the operator does before relying on them.
fn manager_warnings(job: &JobPath) -> Vec<String> {
    let Some(detail) = &job.manager_unreachable else {
        return Vec::new();
    };
    let warning = format!(
        "the service manager's environment was not read ({detail}), so the store variables it may set, such as CLAUDE_CONFIG_DIR, are not applied: native references are judged at the default store locations. A conversation stored under a variable-set location may be reported missing (already_not_resumable or would_lose_recovery), and a conversation present only at the default location may be reported adoptable. {MANAGER_REMEDY}, then run the check again before relying on the verdicts. An upgrade or rollback refuses such a judgement of a stopped session unless --accept-runtime-loss is given."
    );
    tracing::warn!(detail = %detail, "{warning}");
    vec![warning]
}

/// The `PATH` of the daemon job of `config`, which every preflight and runtime
/// spawn of the CLI gets.
///
/// The recorded `PATH` of `config` is the job's `PATH`, as
/// [`super::definition::daemon_definition`] writes it into the systemd unit or
/// the launchd plist, except on Linux where the last drop-in of the daemon unit
/// that sets `PATH` replaces it ([`PathSource::DropIn`]). A job that records none
/// inherits its service manager's `PATH`:
///
/// - Linux: the systemd user manager's `Environment`, read now, or a drop-in's
///   `PATH`. The restored or new job gets exactly that, so no journal value
///   applies.
/// - macOS, in launchd's order: the domain's `environment` `PATH` (readable
///   whether or not the job is loaded); the loaded job's `default environment`;
///   for a job that is not loaded, `journal_default` (the launchd default the
///   upgrade journaled while the job was loaded) while the host has not
///   rebooted since, otherwise the persisted configuration; and only without that, the
///   persisted launchd configuration, then launchd's standard `PATH`. Without a
///   journaled default the persisted configuration is
///   [`PathSource::PersistedActive`] when it is older than the host boot;
///   otherwise a forward judgement refuses and a rollback
///   ([`PathUse::Rollback`]) goes on with it, or with launchd's standard `PATH`
///   when it cannot be read, as [`PathSource::Persisted`]. After a reboot a
///   journaled default gives way to the persisted configuration, which is then
///   [`PathSource::PersistedActive`] when it is older than the boot; when that
///   cannot be shown a forward judgement refuses and a rollback goes on as
///   [`PathSource::Persisted`] with the same stand-ins.
///
/// `journal_default` is the journal's `launchd_default_path` with its boot
/// identity, for the transaction the call belongs to, or `None` outside one.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when the service manager's `PATH` cannot
/// be read or cannot be carried by a process environment; on Linux also when the
/// manager is unreachable (a recorded `PATH` included), the daemon unit is masked
/// or failed, or a drop-in or `EnvironmentFile=` removes or cannot be followed
/// for `PATH`, a bootstrap key (`HOME`, `XDG_*`) or a selected store variable;
/// on macOS when a forward judgement of an unloaded job cannot tell which `PATH`
/// launchd applies.
pub(crate) async fn job_path(
    context: &Context,
    config: &ServiceConfig,
    journal_default: Option<JournalDefault<'_>>,
) -> Result<JobPath, Error> {
    job_path_for(context, config, journal_default, PathUse::Forward).await
}

/// [`job_path`] for the given side of a transaction.
///
/// # Errors
///
/// See [`job_path`].
pub(crate) async fn job_path_for(
    context: &Context,
    config: &ServiceConfig,
    journal_default: Option<JournalDefault<'_>>,
    usage: PathUse,
) -> Result<JobPath, Error> {
    recorded_job_path(
        context,
        config.search_path(),
        config.environment_allowlist(),
        journal_default,
        usage,
    )
    .await
}

/// The `PATH` a daemon job whose `service.toml` records `config`'s search path
/// gets, without the manager variables. Callers that need only the `PATH` use
/// this: a recorded `PATH` reads no service manager at all.
///
/// # Errors
///
/// See [`job_path`].
pub(crate) async fn job_search_path(
    context: &Context,
    config: &ServiceConfig,
    journal_default: Option<JournalDefault<'_>>,
) -> Result<JobPath, Error> {
    recorded_job_search_path(context, config.search_path(), journal_default).await
}

/// The `PATH` of a daemon job that records `recorded`, as [`job_search_path`]
/// gives it. Off Linux a recorded `PATH` reads no service manager at all.
///
/// # Errors
///
/// See [`job_path`].
pub(crate) async fn recorded_job_search_path(
    context: &Context,
    recorded: &SearchPath,
    journal_default: Option<JournalDefault<'_>>,
) -> Result<JobPath, Error> {
    // On Linux the daemon unit's drop-ins can replace even a recorded `PATH`, so
    // the same resolver answers every caller.
    if !cfg!(target_os = "linux") && !recorded.is_empty() {
        return Ok(JobPath {
            value: recorded.to_env_value(),
            source: PathSource::Recorded,
            path_drop_in: None,
            loaded_default: None,
            variables: BTreeMap::new(),
            manager_unreachable: None,
        });
    }
    recorded_job_path(context, recorded, &[], journal_default, PathUse::Forward).await
}

/// The `PATH` and the allowlisted manager variables of a daemon job whose
/// `service.toml` records `recorded` and `allowlist`, by the rules of
/// [`job_path`]. A fresh install records the search path
/// [`Context::install_search_path`] resolves, so this is the `PATH` its job
/// will get before anything is written.
///
/// # Errors
///
/// See [`job_path`].
pub(crate) async fn recorded_job_path(
    context: &Context,
    recorded: &SearchPath,
    allowlist: &[String],
    journal_default: Option<JournalDefault<'_>>,
    usage: PathUse,
) -> Result<JobPath, Error> {
    if cfg!(target_os = "linux") {
        return linux_job_path(context, recorded, allowlist).await;
    }
    if !recorded.is_empty() {
        // The recorded `PATH` does not depend on the manager, so an unreachable
        // manager only means its variables are not applied; see `manager_warnings`.
        let ManagerVariables {
            variables,
            unreachable,
        } = manager_variables(context, allowlist).await?;
        return Ok(JobPath {
            value: recorded.to_env_value(),
            source: PathSource::Recorded,
            path_drop_in: None,
            loaded_default: None,
            variables,
            manager_unreachable: unreachable,
        });
    }
    launchd_job_path(context, journal_default, allowlist, usage).await
}

/// The `PATH` and the allowlisted variables of a daemon job on Linux, from the
/// systemd user manager's environment and the daemon unit's own settings and
/// drop-ins (see [`manager_env::effective_job_environment`]).
///
/// The manager and the unit are read now, once per call: a restored or new job
/// gets exactly what they report, so no journal value applies, and no gate
/// reuses another gate's reading. A manager that cannot answer is an error,
/// whether or not the job records a `PATH`: its unit's drop-ins could replace
/// either. The unit's name is derived without creating the application roots: a
/// preview changes nothing.
async fn linux_job_path(
    context: &Context,
    recorded: &SearchPath,
    allowlist: &[String],
) -> Result<JobPath, Error> {
    fn failed(failure: manager_env::EnvironmentFailure) -> Error {
        let (manager_env::EnvironmentFailure::Unreachable(detail)
        | manager_env::EnvironmentFailure::Unusable(detail)) = failure;
        Error::ManagerEnvironment { detail }
    }
    let entries = manager_env::read_user_manager_entries(context.user_manager())
        .await
        .map_err(failed)?;
    let unit_name = context.namespace_without_creating()?.daemon_unit();
    let unit = context
        .user_manager()
        .unit_environment(&unit_name)
        .await
        .map_err(failed)?;
    let recorded_value = (!recorded.is_empty()).then(|| recorded.to_env_value());
    let environment = manager_env::effective_job_environment(
        &entries,
        &unit,
        &unit_name,
        allowlist,
        recorded_value.as_deref(),
    )
    .await?;
    let source = match (&recorded_value, &environment.search_path_drop_in) {
        (Some(_), None) => PathSource::Recorded,
        (Some(_), Some(_)) => PathSource::DropIn,
        (None, _) => PathSource::UserManager,
    };
    Ok(JobPath {
        value: environment.search_path,
        source,
        path_drop_in: environment.search_path_drop_in,
        loaded_default: None,
        variables: environment.variables,
        manager_unreachable: None,
    })
}

/// The allowlisted variables a recorded-`PATH` job takes from its service
/// manager, and the reason they are absent when no manager is reachable.
pub(crate) struct ManagerVariables {
    /// The variables the manager's environment selects, empty when it is not read.
    pub(crate) variables: BTreeMap<String, String>,
    /// Why the manager's environment was not read, when no manager is reachable.
    pub(crate) unreachable: Option<String>,
}

/// The allowlisted variables the service manager gives the job when the job
/// records its `PATH`: the launchd domain's environment on macOS, and none where
/// no service manager exists. Linux reads its unit as well; see
/// [`linux_job_path`].
///
/// A launchd domain without a GUI session reports no environment, which is not a
/// failure; a launchctl that cannot run is one.
/// Only the domain is read: the job's label would derive the namespace, which
/// creates the state and runtime roots, and a recorded `PATH` needs none of that.
#[cfg(target_os = "macos")]
async fn manager_variables(
    context: &Context,
    allowlist: &[String],
) -> Result<ManagerVariables, Error> {
    let domain = context
        .launchd_job()
        .domain_environment(context.uid())
        .await
        .map_err(|detail| Error::ManagerEnvironment { detail })?;
    match domain {
        Some(domain) => Ok(ManagerVariables {
            variables: manager_env::allowlisted_variables(domain, allowlist)?,
            unreachable: None,
        }),
        // Without a GUI session launchd has no domain to read, so the store
        // variables it may set are not applied.
        None => Ok(ManagerVariables {
            variables: BTreeMap::new(),
            unreachable: Some(no_launchd_domain(context)),
        }),
    }
}

/// Linux resolves its unit in [`linux_job_path`], and no other host has a
/// service manager that gives a job variables.
#[cfg(not(target_os = "macos"))]
fn manager_variables(
    _context: &Context,
    _allowlist: &[String],
) -> std::future::Ready<Result<ManagerVariables, Error>> {
    std::future::ready(Ok(ManagerVariables {
        variables: BTreeMap::new(),
        unreachable: None,
    }))
}

/// The launchd `PATH` of a job without its own (macOS), in launchd's order: the
/// domain's `PATH`; the loaded job's default; for a job that is not loaded, the
/// default the transaction journaled; and only without that, the persisted
/// configuration, then launchd's standard `PATH`.
#[cfg(target_os = "macos")]
async fn launchd_job_path(
    context: &Context,
    journal_default: Option<JournalDefault<'_>>,
    allowlist: &[String],
    usage: PathUse,
) -> Result<JobPath, Error> {
    use super::manager_env::ActiveJob;

    let ActiveJob {
        domain,
        loaded_default,
        domain_environment,
    } = active_launchd_job(context).await?;
    let (variables, unreachable) = match domain_environment {
        Some(entries) => (
            manager_env::allowlisted_variables(entries, allowlist)?,
            None,
        ),
        None => (BTreeMap::new(), Some(no_launchd_domain(context))),
    };
    let (value, source) = if let Some(value) = domain {
        (value, PathSource::Domain)
    } else if let Some(value) = loaded_default.clone() {
        (value, PathSource::LoadedDefault)
    } else if let Some(journal) = journal_default {
        journaled_or_persisted_path(context, journal, usage)?
    } else {
        unloaded_persisted_path(context, usage)?
    };
    let value =
        manager_env::carry(&value).map_err(|detail| Error::ManagerEnvironment { detail })?;
    Ok(JobPath {
        value,
        source,
        path_drop_in: None,
        loaded_default,
        variables,
        manager_unreachable: unreachable,
    })
}

/// The `PATH` of the persisted launchd user-domain configuration, which a
/// reboot makes active (`launchctl config user path`), or launchd's standard one.
#[cfg(target_os = "macos")]
fn persisted_user_domain_path(context: &Context) -> Result<String, Error> {
    pohunek_platform::supervisor::launchd::read_user_domain_path(context.launchd_domain_config())
        .map_err(|error| Error::ManagerEnvironment {
            detail: format!("the launchd user domain's PATH cannot be read: {error}"),
        })
}

/// The `PATH` of a job that is not loaded when no transaction journaled a
/// launchd default: the persisted user-domain configuration, which launchd
/// applies at boot.
///
/// The persisted value is the active one only when `user.plist` was last
/// modified before the host booted, by more than the configured margin that
/// keeps a clock step around the boot from reordering the two times: a later
/// write is pending, or applied and changed again. An absent file is dated by
/// its directory (a change of the directory after the boot may be the removal of
/// a file launchd applied, so the absent file is not certain to mean the standard
/// `PATH`), and launchd's standard `PATH` then counts as persisted. A
/// configuration that is not older, or whose time or the boot time cannot be
/// read, may have a change pending: a forward gate refuses, and a rollback goes
/// on with the persisted value as a stand-in and says so, because refusing a
/// rollback would strand the host. A configuration whose content cannot be read
/// refuses a forward gate, and a rollback goes on with launchd's standard `PATH`
/// as the stand-in.
#[cfg(target_os = "macos")]
fn unloaded_persisted_path(
    context: &Context,
    usage: PathUse,
) -> Result<(String, PathSource), Error> {
    let dated = persisted_config_is_active(context);
    let persisted = match (persisted_user_domain_path(context), usage) {
        (Ok(persisted), _) => persisted,
        // The configuration cannot be read, so its value is unknown: launchd's
        // standard `PATH` stands in, as it does for a missing file.
        (Err(error), PathUse::Rollback) => {
            tracing::warn!(%error, "the launchd user-domain configuration cannot be read");
            return Ok((
                pohunek_platform::supervisor::launchd::STANDARD_PATH.to_owned(),
                PathSource::Persisted,
            ));
        }
        (Err(error), PathUse::Forward) => return Err(error),
    };
    // A write between the dating and the read would make the date describe
    // another configuration than the one read.
    let activity = dated.and_then(|()| persisted_config_is_active(context));
    match (activity, usage) {
        (Ok(()), _) => Ok((persisted, PathSource::PersistedActive)),
        (Err(_), PathUse::Rollback) => Ok((persisted, PathSource::Persisted)),
        (Err(reason), PathUse::Forward) => Err(Error::ManagerEnvironment {
            detail: format!(
                "which PATH launchd applies to the daemon job, which is not loaded, cannot be told ({reason}). Start the daemon job so launchd loads it, set the PATH explicitly with `launchctl setenv PATH <directories>` in the GUI session, or reboot the host, and retry"
            ),
        }),
    }
}

/// Whether the persisted launchd user-domain configuration is older than the
/// current boot, so launchd has applied it; the reason otherwise.
#[cfg(target_os = "macos")]
fn persisted_config_is_active(context: &Context) -> Result<(), String> {
    let booted = context
        .launchd_job()
        .boot_time()
        .map_err(|error| format!("the boot time cannot be read: {error}"))?;
    let date = || {
        pohunek_platform::supervisor::launchd::user_domain_config_modified(
            context.launchd_domain_config(),
        )
        .map_err(|error| format!("the launchd user-domain configuration cannot be dated: {error}"))
    };
    let changed = date()?;
    // Both times are on the calendar clock, and a clock step right after the boot
    // moves `kern.boottime` but not the times of files written before the step.
    let settled = booted
        .checked_sub(super::settings::LAUNCHD_CONFIG_BOOT_MARGIN)
        .unwrap_or(std::time::UNIX_EPOCH);
    if changed >= settled {
        return Err(
            "the launchd user-domain configuration was modified after, or too close to, the host boot, so a change may be pending or may have been applied and changed again"
                .to_owned(),
        );
    }
    Ok(())
}

/// The journaled launchd default of a job that is not loaded, or the persisted
/// configuration when the host rebooted since it was journaled.
///
/// The default was read from the loaded job's environment, which a pending
/// `launchctl config user path` replaces at the next boot. After a boot change
/// (a different kernel boot session UUID, which a clock change leaves alone) the
/// persisted value is the active one only when `user.plist` was last modified
/// before the current boot: a later write is pending, or applied and changed
/// again, and equality with any earlier value proves nothing about it. Without a
/// boot marker, or when the current boot identity cannot be read, the same time
/// rule applies and the persisted value must also equal the journaled one.
///
/// A configuration that fails the rule, or whose time, content or the boot time
/// cannot be read, refuses a forward gate. A rollback goes on with the persisted
/// value as a stand-in, or with launchd's standard `PATH` when the content cannot
/// be read, and says so, because refusing a rollback would strand the host.
#[cfg(target_os = "macos")]
fn journaled_or_persisted_path(
    context: &Context,
    journal: JournalDefault<'_>,
    usage: PathUse,
) -> Result<(String, PathSource), Error> {
    let current = context.launchd_job().boot_identity();
    // The same boot needs no look at the persisted configuration, so an
    // unreadable one must not block it.
    if let (Some(journaled), Ok(now)) = (journal.boot_identity, &current) {
        if journaled == now {
            return Ok((journal.path.to_owned(), PathSource::Journal));
        }
    }
    match active_persisted_path_after_reboot(context, journal, &current) {
        Ok(persisted) => Ok((persisted, PathSource::PersistedActive)),
        Err(error) if usage == PathUse::Rollback => {
            tracing::warn!(%error, "the launchd PATH of the unloaded job cannot be told");
            let persisted = persisted_user_domain_path(context).unwrap_or_else(|error| {
                tracing::warn!(%error, "the launchd user-domain configuration cannot be read");
                pohunek_platform::supervisor::launchd::STANDARD_PATH.to_owned()
            });
            Ok((persisted, PathSource::Persisted))
        }
        Err(error) => Err(error),
    }
}

/// The persisted launchd configuration when it is provably the active one after
/// a boot change, or the reason it is not, as the forward refusal.
#[cfg(target_os = "macos")]
fn active_persisted_path_after_reboot(
    context: &Context,
    journal: JournalDefault<'_>,
    current: &Result<String, String>,
) -> Result<String, Error> {
    let boot_changed = journal.boot_identity.is_some() && current.is_ok();
    let refuse = |reason: String| {
        Error::ManagerEnvironment {
        detail: format!(
            "which PATH launchd applies to the daemon job cannot be told ({reason}). Set the PATH explicitly with `launchctl setenv PATH <directories>` in the GUI session and retry"
        ),
    }
    };
    let booted = context
        .launchd_job()
        .boot_time()
        .map_err(|error| refuse(format!("the boot time cannot be read: {error}")))?;
    let date = || {
        pohunek_platform::supervisor::launchd::user_domain_config_modified(
            context.launchd_domain_config(),
        )
        .map_err(|error| {
            refuse(format!(
                "the launchd user-domain configuration cannot be dated: {error}"
            ))
        })
    };
    let changed = date()?;
    // Both times are on the calendar clock: a clock step right after the boot
    // moves `kern.boottime` but not the times of files written before the
    // step, so a write inside that window can look older than the boot.
    if changed >= booted {
        return Err(refuse(
            "the launchd user-domain configuration was modified after the host booted, so it may be pending or may have been applied and changed again".to_owned(),
        ));
    }
    let persisted = persisted_user_domain_path(context)?;
    // A write between the dating and the read would make the date describe
    // another configuration than the one read.
    if date()? != changed {
        return Err(refuse(
            "the launchd user-domain configuration changed while it was read".to_owned(),
        ));
    }
    if !boot_changed && persisted != journal.path {
        let why = if journal.boot_identity.is_none() {
            "the journal has no boot marker".to_owned()
        } else {
            format!(
                "the current boot identity cannot be read ({})",
                current.as_ref().err().cloned().unwrap_or_default()
            )
        };
        return Err(refuse(format!(
            "the persisted configuration differs from the PATH the upgrade journaled and {why}"
        )));
    }
    Ok(persisted)
}

/// The boot identity to journal next to a launchd default, or `None` with a
/// warning when it cannot be read: the journal then proves nothing about reboots.
#[cfg(target_os = "macos")]
fn journaled_boot_identity(context: &Context) -> Option<String> {
    match context.launchd_job().boot_identity() {
        Ok(identity) => Some(identity),
        Err(error) => {
            tracing::warn!(%error, "the boot identity cannot be read, so the journaled launchd PATH carries no boot marker");
            None
        }
    }
}

/// Only macOS journals a launchd default.
#[cfg(not(target_os = "macos"))]
fn journaled_boot_identity(_context: &Context) -> Option<String> {
    None
}

/// Why launchd's variables were not read: it has no user domain for `context`'s
/// user, which is the case without a GUI session.
#[cfg(target_os = "macos")]
fn no_launchd_domain(context: &Context) -> String {
    format!(
        "launchd has no user domain for uid {} (no GUI session)",
        context.uid()
    )
}

/// The launchd state of the daemon job of `context`.
#[cfg(target_os = "macos")]
async fn active_launchd_job(context: &Context) -> Result<super::manager_env::ActiveJob, Error> {
    let label = context.namespace()?.daemon_label();
    context
        .launchd_job()
        .active_path(context.uid(), &label)
        .await
        .map_err(|detail| Error::ManagerEnvironment { detail })
}

/// Only macOS has launchd.
#[cfg(not(target_os = "macos"))]
fn launchd_job_path<'a>(
    _context: &'a Context,
    _journal_default: Option<JournalDefault<'a>>,
    _allowlist: &'a [String],
    _usage: PathUse,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JobPath, Error>> + Send + 'a>> {
    Box::pin(std::future::ready(Err(Error::ManagerEnvironment {
        detail: "a launchd job exists only on macOS".to_owned(),
    })))
}

/// Refuses an upgrade whose configuration rendering exceeds the load bound.
///
/// Kept unknown keys can make a rendering larger than the file it came from.
/// Every rendering the plan writes is checked before the first effect, so
/// neither the upgrade nor its rollback can hit the bound later. A rollback
/// writes only when the file already left the previous version, and writes the
/// journaled original bytes for a schema migration; an upgrade to the active
/// version rewrites nothing.
///
/// # Errors
///
/// Returns [`ConfigError::TooLarge`] without changing anything.
pub(crate) fn check_planned_renderings(
    config: &ServiceConfig,
    version: &str,
    plan: &Plan,
    path: &Path,
) -> Result<(), Error> {
    let mut resuming = false;
    let mut after_rollback = config.clone();
    match plan {
        Plan::Fresh => {}
        Plan::Resume(_) => resuming = true,
        Plan::RollBack(record) => {
            if let Some(previous) = &record.previous_version {
                after_rollback = with_version(config, previous)?;
                if record.config_backup.is_none() && config.active_version() != previous {
                    check_rendering_size(&after_rollback, path)?;
                }
            }
        }
    }
    check_upgrade_renderings(&after_rollback, version, resuming, path)
}

/// Refuses an upgrade whose current or target rendering exceeds the bound.
pub(crate) fn check_upgrade_renderings(
    config: &ServiceConfig,
    version: &str,
    resuming: bool,
    path: &Path,
) -> Result<(), Error> {
    if !resuming && config.active_version() == version {
        return Ok(());
    }
    check_rendering_size(config, path)?;
    check_rendering_size(&with_version(config, version)?, path)
}

fn check_rendering_size(config: &ServiceConfig, path: &Path) -> Result<(), Error> {
    if config.to_toml().len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
            max_bytes: MAX_CONFIG_BYTES,
        }
        .into());
    }
    Ok(())
}

/// Checks a pending configuration migration without changing the file or job.
///
/// `service check`, upgrade resume, and rollback use the same journal and
/// on-disk byte rules. A valid current config alone cannot prove that the
/// previous daemon's exact config can be restored.
/// At `StopDaemon`, the config can be either the old bytes or the rendered
/// target: a crash may have happened after the atomic write and before its
/// checkpoint. Later forward steps require the target body, whatever header
/// text the writing release put above it; a rollback can
/// also have restored the exact old bytes. Any third content is refused.
///
/// # Errors
///
/// Returns [`Error::ConfigBackup`] without changing anything.
pub(crate) fn verify_pending_migration(
    context: &Context,
    store: &Store,
    record: &Record,
    source: &UpgradeSource,
) -> Result<(), Error> {
    let Some(backup) = &record.config_backup else {
        return Ok(());
    };
    if record.step >= Step::BackedUp {
        let bytes = store
            .read_config_backup()?
            .ok_or_else(|| backup_error(store.config_backup_path(), "is missing"))?;
        if config_digest(&bytes) != backup.digest {
            return Err(backup_error(
                store.config_backup_path(),
                "does not match the digest journaled in the transaction record",
            ));
        }
        if record.step >= Step::StopDaemon {
            let previous =
                ServiceConfig::load_upgrade_source_bytes(&store.config_backup_path(), &bytes)?;
            if previous.source_schema_version != backup.source_schema_version {
                return Err(backup_error(
                    store.config_backup_path(),
                    "does not match the schema journaled in the transaction record",
                ));
            }
            let target = with_version(&previous.config, &record.version)?;
            if body_after_header(source.original_bytes())
                == body_after_header(target.to_toml().as_bytes())
                || (source.original_bytes() == bytes
                    && (record.step == Step::StopDaemon || record.rolling_back))
            {
                return Ok(());
            }
            return Err(backup_error(
                context.config_path(),
                "does not match the exact configuration journaled for this migration step",
            ));
        }
    }
    verify_migration_source(context, record, backup, source)
}

/// Checks the pre-rewrite source against the bytes journaled for a migration.
///
/// A moved, edited, or replaced source fails before it can be backed up or
/// rewritten.
fn verify_migration_source(
    context: &Context,
    record: &Record,
    backup: &ConfigBackup,
    source: &UpgradeSource,
) -> Result<(), Error> {
    if record.step < Step::Config {
        let detail = if digest_mismatch(source.original_bytes(), backup) {
            format!(
                "does not match the digest journaled in the transaction record ({})",
                backup.digest
            )
        } else if source.source_schema_version != backup.source_schema_version {
            format!(
                "is schema {} now, but the transaction recorded schema {}",
                source.source_schema_version, backup.source_schema_version
            )
        } else {
            return Ok(());
        };
        return Err(backup_error(context.config_path(), detail));
    }
    Ok(())
}

/// The migration backup description of an upgrade transaction.
///
/// A fresh transaction derives it from the file it read: a source schema
/// other than the current one makes the transaction migrate, and the digest
/// binds the journal to the exact bytes it backs. A resumed transaction
/// keeps the record's description.
fn config_backup_of(resume: Option<&Record>, source: &UpgradeSource) -> Option<ConfigBackup> {
    resume
        .and_then(|record| record.config_backup.clone())
        .or_else(|| {
            (source.source_schema_version != SCHEMA_VERSION).then_some(ConfigBackup {
                source_schema_version: source.source_schema_version,
                digest: config_digest(source.original_bytes()),
            })
        })
}

/// Hexadecimal SHA-256 of `bytes`, the digest a migration journal binds its
/// backup to.
pub(super) fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut text = String::with_capacity(2 * Sha256::output_size());
    for byte in Sha256::digest(bytes) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Whether `bytes` no longer hash to a migration journal's digest.
fn digest_mismatch(bytes: &[u8], backup: &ConfigBackup) -> bool {
    config_digest(bytes) != backup.digest
}

/// Builds the [`Error::ConfigBackup`] with `detail`.
fn backup_error(path: PathBuf, detail: impl Into<String>) -> Error {
    Error::ConfigBackup {
        path,
        detail: detail.into(),
    }
}

pub(super) fn file_exists(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_metadata) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(super::error::io_error("inspect", path)(error)),
    }
}

/// Removes only producer-recognized artifacts through retained directory descriptors.
fn purge_store_artifacts(
    data: &pohunek_platform::filesystem::TrustedDir,
    report: &mut UninstallReport,
) -> Result<(), Error> {
    purge_recognized_entries(
        data,
        report,
        |name| pohunek_paths::is_metadata_store_temp(pohunek_paths::METADATA_STORE_NAME, name),
        false,
    )?;
    let migrations = match data
        .open_child_owner_safe(pohunek_paths::MIGRATIONS_SUBDIR, layout::FORBIDDEN_BITS)
    {
        Ok(directory) => directory,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => return Ok(()),
        Err(source) => return Err(super::error::fs_error("open migrations for purge", source)),
    };
    purge_recognized_entries(
        &migrations,
        report,
        pohunek_paths::is_imported_manifest_archive,
        true,
    )
}

fn purge_recognized_entries(
    directory: &pohunek_platform::filesystem::TrustedDir,
    report: &mut UninstallReport,
    recognized: impl Fn(&str) -> bool,
    report_kept: bool,
) -> Result<(), Error> {
    let mut names = directory
        .entry_names()
        .map_err(|source| super::error::fs_error("list purge artifacts", source))?;
    names.sort();
    for name in names {
        if let Some(name) = name.to_str().filter(|name| recognized(name)) {
            record::remove_purge_artifact(directory, name)?;
            report.removed.push(directory.path().join(name));
        } else if report_kept {
            report.kept_purge_entries.push(directory.path().join(name));
        }
    }
    Ok(())
}

/// Names of the schema-migration backups of `store` in `data`.
///
/// Only names matching the daemon's backup grammar exactly are returned; the
/// caller removes them through descriptor-relative, no-follow operations.
fn schema_backups(
    data: &pohunek_platform::filesystem::TrustedDir,
    store: &str,
) -> Result<Vec<String>, Error> {
    let names = data
        .entry_names()
        .map_err(|source| super::error::fs_error("list data directory", source))?;
    let mut backups: Vec<String> = names
        .iter()
        .filter_map(|name| name.to_str())
        .filter(|name| pohunek_paths::is_schema_backup_artifact(store, name))
        .map(str::to_owned)
        .collect();
    backups.sort();
    Ok(backups)
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
