//! Typed failures of `pohunek service`.

// Rust guideline compliant 2026-10-08

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::FsError;
use pohunek_platform::supervisor;
use pohunek_service_config::preflight::SessionVerdict;
use pohunek_service_config::runtimes::Refusal;

use super::report::AcceptanceGroups;

/// One session that blocks an uninstall.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LiveSession {
    /// Logical session ID.
    pub id: String,
    /// Owner-set display name, when one is set.
    pub name: Option<String>,
}

impl std::fmt::Display for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{} ({name})", self.id),
            None => f.write_str(&self.id),
        }
    }
}

/// A journal of an older schema whose worker may still run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OutdatedWorker {
    /// Journal file.
    pub path: PathBuf,
    /// Schema version the journal records.
    pub schema_version: u32,
    /// Recorded worker process ID, when the journal holds a readable one.
    pub pid: Option<u32>,
}

impl std::fmt::Display for OutdatedWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (schema {}, ",
            self.path.display(),
            self.schema_version
        )?;
        match self.pid {
            Some(pid) => write!(f, "worker pid {pid})"),
            None => f.write_str("no readable worker pid)"),
        }
    }
}

/// A failure of an install, upgrade, uninstall, or status operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A required environment variable is missing.
    #[error("required environment variable {var} is not set (no safe default exists)")]
    MissingEnv {
        /// The missing variable.
        var: String,
    },

    /// The shared path contract rejected the environment.
    #[error("invalid application path configuration: {0}")]
    Paths(#[source] pohunek_paths::PathError),

    /// A bootstrap environment root is not UTF-8.
    ///
    /// The daemon's job definition carries UTF-8 values only, and a daemon
    /// started without the variable would resolve another root from `HOME`.
    #[error(
        "{var} is not UTF-8 ({}), so the daemon's job definition cannot pass it on unchanged",
        path.display()
    )]
    NonUtf8Env {
        /// The environment variable naming the root.
        var: &'static str,
        /// The rejected root.
        path: PathBuf,
    },

    /// A bootstrap environment variable the daemon would refuse at startup.
    ///
    /// The daemon applies the job-definition rules to every bootstrap
    /// variable and starts each session worker in `HOME`, so it never becomes
    /// ready with such a value.
    #[error("{var} ({}) is not usable by the daemon and its session workers: {detail}", path.display())]
    UnusableEnv {
        /// The environment variable.
        var: String,
        /// The rejected value.
        path: PathBuf,
        /// Which rule the value breaks.
        detail: String,
    },

    /// `--prefix` or `--from` is not an absolute normalized path.
    #[error("{flag} must be an absolute normalized path: {}", path.display())]
    InvalidPath {
        /// The flag carrying the path.
        flag: &'static str,
        /// The rejected path.
        path: PathBuf,
    },

    /// No configured, discovered, or fallback directory can serve as the
    /// daemon's executable search path.
    #[error("cannot determine an executable search path for the daemon job: {0}")]
    SearchPath(#[from] pohunek_platform::shell_env::ResolveError),

    /// The `service.toml` an interrupted install wrote names another
    /// installation than the one being resumed.
    #[error(
        "service.toml records {key} = {recorded}, but the interrupted install being resumed \
         uses {expected}; it was changed or belongs to another installation"
    )]
    ResumeConfigMismatch {
        /// The mismatching key.
        key: &'static str,
        /// The value in `service.toml`.
        recorded: String,
        /// The value of the interrupted install.
        expected: String,
    },

    /// Reading or writing `service.toml` failed.
    #[error(transparent)]
    Config(#[from] pohunek_service_config::ConfigError),

    /// A directory a service operation depends on lacks the ownership or
    /// mode the check requires: it lets other users write to it, it does not
    /// have the exact owner-private mode the check demands, it is owned by
    /// another user, or it has an unsafe extended ACL.
    #[error("{} is not a trusted directory: {detail}; {fix}", path.display())]
    UntrustedDirectory {
        /// The directory that failed the ownership or mode check.
        path: PathBuf,
        /// What exactly was wrong with it.
        detail: String,
        /// The remediation the message names, rendered verbatim.
        fix: String,
    },

    /// A trusted filesystem operation failed.
    #[error("{operation} failed: {source}")]
    Filesystem {
        /// What the installer was doing.
        operation: &'static str,
        /// The filesystem failure.
        #[source]
        source: FsError,
    },

    /// A plain filesystem operation failed.
    #[error("failed to {operation} {}: {source}", path.display())]
    Io {
        /// What the installer was doing.
        operation: &'static str,
        /// The affected path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: io::Error,
    },

    /// A command failed to spawn; terminal recovery observations accompany it.
    #[error("failed to run {}: {source}; terminal recovery: {recovery}", path.display())]
    CommandSpawn {
        /// The program that could not execute.
        path: PathBuf,
        /// The original spawn error, retained with its OS error code.
        #[source]
        source: io::Error,
        /// Ownership observations and raw errno values of the attempted recovery.
        recovery: String,
    },

    /// A staged binary is missing or not a regular file.
    #[error("staged binary {} is missing or not a regular file", path.display())]
    StagedBinary {
        /// The expected staged binary.
        path: PathBuf,
    },

    /// The staged catalog trust anchor is present but cannot be installed.
    #[error("staged catalog trust anchor {} is unusable: {detail}", path.display())]
    StagedAnchor {
        /// The staged anchor file.
        path: PathBuf,
        /// Why it was refused; never file content.
        detail: String,
    },

    /// The staged release `runtime/` tree is present but cannot be installed.
    #[error("staged runtime tree {} is unusable: {detail}", path.display())]
    StagedRuntime {
        /// The staged entry that was refused.
        path: PathBuf,
        /// Why it was refused; never file content.
        detail: String,
    },

    /// A staged binary's `--version` could not be established.
    #[error("could not read the version of {}: {detail}", binary.display())]
    VersionProbe {
        /// The probed binary.
        binary: PathBuf,
        /// Why the probe failed.
        detail: String,
    },

    /// A staged binary reports another version than this CLI.
    #[error(
        "{} reports version {found:?}, but this `pohunek` is {expected}; stage binaries from one build",
        binary.display()
    )]
    VersionMismatch {
        /// The probed binary.
        binary: PathBuf,
        /// This CLI's version.
        expected: String,
        /// The reported version line.
        found: String,
    },

    /// An installed version directory holds different binaries.
    #[error(
        "{} already exists with different binaries; a version directory is never overwritten",
        path.display()
    )]
    VersionConflict {
        /// The existing version directory.
        path: PathBuf,
    },

    /// An installed version directory or one of its binaries is not exactly
    /// what staging and publishing create: owned by this user, mode `0755`,
    /// no symbolic link, and each binary a single-link regular file.
    ///
    /// The service manager executes these binaries later, so an entry another
    /// account could replace is never reused.
    #[error("{} cannot be trusted as an installed version: {detail}", path.display())]
    UntrustedVersion {
        /// The offending version directory or binary.
        path: PathBuf,
        /// What exactly was wrong with it.
        detail: String,
    },

    /// The active version directory cannot be trusted, so the daemon cannot be
    /// started to stop the live worker jobs an uninstall found.
    ///
    /// Worker jobs end only through the daemon; recovering the directory is
    /// the way out, and the uninstall is rerun afterwards. Carries the code of
    /// [`Error::UntrustedVersion`].
    #[error(
        "the active version {version} cannot be trusted ({}: {detail}), so the daemon cannot start and the live worker jobs cannot be stopped through it",
        path.display()
    )]
    UntrustedActiveVersion {
        /// The active version.
        version: String,
        /// The offending version directory or binary.
        path: PathBuf,
        /// What exactly was wrong with it.
        detail: String,
    },

    /// The version directory a pending search-path refresh restarts the daemon
    /// from, forward or in its rollback, cannot be trusted.
    ///
    /// Carries the code of [`Error::UntrustedVersion`]. `upgrade` and
    /// `install` refuse the pending refresh, so the way out is the two-step
    /// sequence the hint names.
    #[error(
        "the daemon of the pending search-path refresh runs version {version}, which cannot be trusted ({}: {detail}), so the refresh can neither finish nor be rolled back",
        path.display()
    )]
    UntrustedRefreshVersion {
        /// The version the refresh restarts.
        version: String,
        /// The offending version directory or binary.
        path: PathBuf,
        /// What exactly was wrong with it.
        detail: String,
    },

    /// The prefix belongs to another installation namespace.
    ///
    /// Version directories and `<prefix>/bin/pohunek` are shared by every
    /// installation using the prefix, while journals, worker jobs, and
    /// transactions are per namespace, so one prefix serves exactly one
    /// namespace.
    #[error(
        "{} belongs to the pohunek installation with namespace {owner} (recorded in {}); \
         two installations never share a prefix",
        prefix.display(),
        record.display()
    )]
    PrefixOwned {
        /// The contested prefix.
        prefix: PathBuf,
        /// The namespace the ownership record names.
        owner: String,
        /// The ownership record.
        record: PathBuf,
    },

    /// `service install` found an existing installation.
    #[error("pohunek is already installed as a service ({})", path.display())]
    AlreadyInstalled {
        /// The existing `service.toml`.
        path: PathBuf,
    },

    /// The operation needs an installation that does not exist.
    #[error("pohunek is not installed as a service ({} is missing)", path.display())]
    NotInstalled {
        /// The missing `service.toml`.
        path: PathBuf,
    },

    /// A command other than the pending install's own `install` found it.
    ///
    /// Only `service install` with the same version and prefix finishes an
    /// interrupted install; once it reached the registration step, its daemon
    /// may already run with live workers, so no other command may roll it
    /// back.
    #[error(
        "an interrupted install of version {version} is pending (last completed step: {step}); \
         it may already run its daemon"
    )]
    PendingInstall {
        /// The version the pending install targets.
        version: String,
        /// The last journaled step of the pending install.
        step: &'static str,
    },

    /// An install failed after asking the service manager to register its daemon.
    ///
    /// The registration call can fail after the job was really registered (a
    /// D-Bus or `launchctl` timeout), and a registered daemon may already own
    /// live sessions. The install is therefore kept pending instead of rolled
    /// back: only a resume with the same version and prefix, or the
    /// session-checked uninstall, may decide what happens to that daemon.
    #[error(
        "{original}; the install of version {version} under {} is kept pending at step {step} \
         because its daemon job may already be registered",
        prefix.display()
    )]
    InstallIncomplete {
        /// The failure that stopped the install.
        original: Box<Error>,
        /// The version the pending install targets.
        version: String,
        /// The prefix the pending install targets.
        prefix: PathBuf,
        /// The last journaled step of the pending install.
        step: &'static str,
    },

    /// A daemon job of this namespace exists without an installation record.
    #[error("daemon job {id} is registered, but no service.toml describes it")]
    DaemonJobPresent {
        /// The registered daemon job.
        id: String,
    },

    /// A native service-manager operation failed.
    #[error("service manager operation `{operation}` failed: {source}")]
    Supervisor {
        /// The operation.
        operation: &'static str,
        /// The backend failure.
        #[source]
        source: supervisor::Error,
    },

    /// `systemd-analyze` is not installed.
    #[error("`systemd-analyze` was not found on PATH; it is required to verify the daemon unit")]
    VerifierMissing,

    /// `systemd-analyze verify` rejected the rendered units.
    #[error("systemd-analyze rejected the daemon unit: {detail}")]
    UnitVerification {
        /// Bounded verifier diagnostics.
        detail: String,
    },

    /// The daemon did not answer as the expected version in time.
    #[error(
        "the daemon did not report version {version} within {} s{}",
        timeout.as_secs(),
        last.as_deref().map(|last| format!(" (last observation: {last})")).unwrap_or_default()
    )]
    DaemonNotReady {
        /// The version the installer waited for.
        version: String,
        /// The readiness bound.
        timeout: std::time::Duration,
        /// The last health or service-manager observation.
        last: Option<String>,
    },

    /// The supervised daemon job serves a version the command may not use.
    ///
    /// A removal reaches only the daemon a pending transaction or the
    /// configuration proves belongs to it; anything else running the job —
    /// for example another installation's daemon swapped in — must not be
    /// stopped or asked for its sessions here. The refusal happens before
    /// any session mutation.
    #[error(
        "the supervised daemon job reports version {served}; only {} is accepted here",
        accepted.join(" or ")
    )]
    SupervisedVersion {
        /// The version the serving process reports.
        served: String,
        /// The versions the command proves belong to this installation.
        accepted: Vec<String>,
    },

    /// A control request to the local daemon failed.
    #[error("daemon request `{operation}` failed: {source}")]
    Control {
        /// The request.
        operation: &'static str,
        /// The client failure, boxed to keep the error type small.
        #[source]
        source: Box<pohunek_client::ClientError>,
    },

    /// Uninstall refused because sessions or workers are live.
    #[error(
        "{}",
        live_summary("uninstall refused: live sessions remain", sessions, workers)
    )]
    LiveSessions {
        /// Live logical sessions.
        sessions: Vec<LiveSession>,
        /// Live worker jobs not covered by a listed session.
        workers: Vec<String>,
    },

    /// Stopped sessions did not settle within the bound.
    #[error("{}", live_summary("sessions did not stop in time", sessions, workers))]
    StopTimeout {
        /// Sessions still live.
        sessions: Vec<LiveSession>,
        /// Worker jobs still live.
        workers: Vec<String>,
    },

    /// Worker journals could not be read, so liveness cannot be proven.
    #[error(
        "worker journals could not be read, so their workers cannot be proven ended: {}",
        paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    UnreadableJournals {
        /// The unreadable journal files.
        paths: Vec<PathBuf>,
    },

    /// Journals of an older schema name workers that may still run.
    #[error(
        "worker journals written by an older pohunek name workers that may still run: {}",
        workers.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
    )]
    OutdatedJournals {
        /// The blocking journals.
        workers: Vec<OutdatedWorker>,
    },

    /// The new daemon would not adopt, or would adopt without native
    /// recovery, live or stopped sessions of the installed state.
    #[error(
        "{}",
        at_risk_summary("upgrade", sessions, stopped_sessions, acceptance, warnings)
    )]
    UpgradeAtRisk {
        /// Live sessions the preflight judged at risk and no earlier upgrade
        /// accepted, ordered by id.
        sessions: Vec<SessionVerdict>,
        /// Stopped sessions whose retained native recovery is at risk and no
        /// earlier upgrade accepted.
        stopped_sessions: Vec<SessionVerdict>,
        /// The at-risk verdicts an earlier upgrade accepted, which do not block.
        acceptance: Box<AcceptanceGroups>,
        /// Warnings of the judgement, so a refusal explains a degraded verdict.
        warnings: Vec<String>,
        /// Whether the refused swap belongs to an upgrade an earlier run left
        /// pending, which the operator can also abandon.
        resumed: bool,
    },

    /// A search-path refresh would restart the daemon so that it does not
    /// adopt, or adopts without native recovery, live or stopped sessions.
    #[error(
        "{}",
        at_risk_summary(
            "search-path refresh",
            sessions,
            stopped_sessions,
            acceptance,
            warnings
        )
    )]
    RefreshAtRisk {
        /// Live sessions the preflight judged at risk and no earlier upgrade
        /// accepted, ordered by id.
        sessions: Vec<SessionVerdict>,
        /// Stopped sessions whose retained native recovery is at risk and no
        /// earlier upgrade accepted.
        stopped_sessions: Vec<SessionVerdict>,
        /// The at-risk verdicts an earlier upgrade accepted, which do not block.
        acceptance: Box<AcceptanceGroups>,
        /// Warnings of the judgement, so a refusal explains a degraded verdict.
        warnings: Vec<String>,
    },

    /// The new daemon would refuse to start with the installed metadata store.
    #[error(
        "upgrade refused: the new daemon cannot start with the metadata store {} ({code}): {detail}",
        path.display()
    )]
    UpgradeStoreUnusable {
        /// The store file.
        path: PathBuf,
        /// Machine code of the store refusal.
        code: String,
        /// The refusal, as the daemon words it.
        detail: String,
    },

    /// The new daemon's preflight did not produce a report.
    #[error("the adoption preflight of {} failed: {detail}", binary.display())]
    UpgradePreflightFailed {
        /// The daemon executable that was asked.
        binary: PathBuf,
        /// Why no report came back.
        detail: String,
    },

    /// The daemon of the installed release would refuse to start with the
    /// metadata store, so a search-path refresh is refused.
    #[error(
        "refused: the daemon of the installed release cannot start with the metadata store {} ({code}): {detail}",
        path.display()
    )]
    RefreshStoreUnusable {
        /// The store file.
        path: PathBuf,
        /// Machine code of the store refusal.
        code: String,
        /// The refusal, as the daemon words it.
        detail: String,
    },

    /// The installed release's daemon preflight did not produce a report for
    /// a search-path refresh.
    #[error("the adoption preflight of {} failed: {detail}", binary.display())]
    RefreshPreflightFailed {
        /// The daemon executable that was asked.
        binary: PathBuf,
        /// Why no report came back.
        detail: String,
    },

    /// The release's official runtime step refused its plan, so nothing was
    /// stopped or changed.
    #[error(
        "upgrade refused: the official runtime packages cannot be installed: {}",
        refusals_summary(refusals)
    )]
    UpgradeRuntimesRefused {
        /// Every refusal the plan carries, in report order.
        refusals: Vec<Refusal>,
    },

    /// The official runtime step did not end its daemon run, or its exit could
    /// not be confirmed, so state may still be changing and nothing is restored.
    #[error("the official runtime step of {} did not end: {detail}", binary.display())]
    UpgradeRuntimesUnconfirmed {
        /// The daemon executable that was asked.
        binary: PathBuf,
        /// Why the exit is not confirmed.
        detail: String,
    },

    /// The `PATH` the daemon job runs with could not be determined, so a
    /// preflight or runtime step cannot run with the daemon's `PATH`.
    #[error("cannot determine the environment the daemon job runs with: {detail}")]
    ManagerEnvironment {
        /// Why no usable `PATH` came back.
        detail: String,
    },

    /// The service manager's environment was not read, so the preflight judged a
    /// stopped session's native reference without the store variables the daemon
    /// job gets. Refused unless the operator accepts runtime loss.
    #[error(
        "the service manager's environment was not read ({detail}), so the preflight cannot judge native references with the store variables the daemon job gets"
    )]
    ManagerVariablesUnread {
        /// Why the manager's environment was not read.
        detail: String,
    },

    /// The official runtime step did not produce a usable report, or its
    /// apply left work undone.
    #[error("the official runtime step of {} failed: {detail}", binary.display())]
    UpgradeRuntimesFailed {
        /// The daemon executable that was asked.
        binary: PathBuf,
        /// Why no usable report came back.
        detail: String,
    },

    /// A runtime backup file is missing, unsafe, or does not match its digest.
    #[error("runtime backup {} is invalid: {detail}", path.display())]
    RuntimeBackup {
        /// The backup file.
        path: PathBuf,
        /// Why it cannot be restored.
        detail: String,
    },

    /// The previous daemon cannot safely read the store after an upgrade.
    #[error("rollback to {version} refused: metadata store {} is unreadable ({code}): {detail}", path.display())]
    RollbackStoreUnusable {
        /// Version that would be restored.
        version: String,
        /// The store file.
        path: PathBuf,
        /// Stable reason code.
        code: String,
        /// Reason the previous reader cannot use the store.
        detail: String,
    },

    /// The previous daemon may lose live runtime or retained native recovery.
    #[error("{}", at_risk_summary(&format!("rollback to {version}"), sessions, stopped_sessions, &AcceptanceGroups::default(), warnings))]
    RollbackAtRisk {
        /// Version that would be restored.
        version: String,
        /// Live sessions requiring explicit acceptance.
        sessions: Vec<SessionVerdict>,
        /// Stopped sessions whose retained native recovery is at risk.
        stopped_sessions: Vec<SessionVerdict>,
        /// Warnings of the judgement, so a refusal explains a degraded verdict.
        warnings: Vec<String>,
    },

    /// Rolling back a search-path refresh would restart the previous daemon
    /// definition so that it may lose live runtime or retained native recovery.
    #[error("{}", at_risk_summary(&format!("rollback of the search-path refresh to {version}"), sessions, stopped_sessions, &AcceptanceGroups::default(), warnings))]
    RefreshRollbackAtRisk {
        /// Version that keeps running.
        version: String,
        /// Live sessions requiring explicit acceptance.
        sessions: Vec<SessionVerdict>,
        /// Stopped sessions whose retained native recovery is at risk.
        stopped_sessions: Vec<SessionVerdict>,
        /// Warnings of the judgement, so a refusal explains a degraded verdict.
        warnings: Vec<String>,
    },

    /// The previous daemon gave no trustworthy rollback verdict.
    #[error("rollback to {version} refused: previous daemon preflight failed: {detail}")]
    RollbackPreflightFailed {
        /// Version that would be restored.
        version: String,
        /// The failed preflight.
        detail: String,
    },

    /// Worker jobs of this namespace remained after retirement.
    #[error("worker jobs remain after uninstall: {}", ids.join(", "))]
    OrphanWorkers {
        /// The remaining jobs.
        ids: Vec<String>,
    },

    /// The install transaction record is unreadable.
    #[error("install transaction record {} is invalid: {detail}", path.display())]
    Record {
        /// The record file.
        path: PathBuf,
        /// Why it was rejected.
        detail: String,
    },

    /// The accepted-risk ledger cannot be read safely.
    #[error("accepted-risk ledger {} is invalid: {detail}", path.display())]
    AcceptedRisks {
        /// The ledger file.
        path: PathBuf,
        /// Why it was rejected.
        detail: String,
    },

    /// A config migration cannot prove the exact pre-upgrade `service.toml`.
    ///
    /// The transaction's owner-private backup and its journaled digest are
    /// the only way to reproduce the bytes the previous daemon reads. A
    /// missing backup, or one whose digest no longer matches the journal,
    /// refuses resumption and rollback closed: nothing is written and no
    /// daemon is restarted, because guessing the pre-upgrade bytes could
    /// strand the installation between two schemas.
    #[error(
        "the interrupted configuration migration cannot continue: the pre-upgrade \
         service.toml backup {} {detail}",
        path.display()
    )]
    ConfigBackup {
        /// The backup file, or the `service.toml` a byte check failed on.
        path: PathBuf,
        /// What is wrong; names the digest journaled in the record.
        detail: String,
    },

    /// `service refresh-path` on a host whose daemon job carries no recorded
    /// `PATH`.
    #[error("this host's daemon job takes the service manager's own PATH, so there is no search path to refresh")]
    SearchPathUnmanaged,

    /// `service refresh-path` on a `service.toml` of another schema than this
    /// release writes.
    #[error("service.toml has schema {found}, but this release refreshes only schema {current}")]
    RefreshConfigSchema {
        /// The `schema_version` the file declares.
        found: i64,
        /// The schema this release reads and writes.
        current: u32,
    },

    /// `service refresh-path` could not read the path from the login shell.
    ///
    /// The fallback list an install would record in that case is never put in
    /// place of a recorded path.
    #[error(
        "the login shell did not supply a PATH ({detail}); the recorded search path is unchanged"
    )]
    SearchPathRefreshFailed {
        /// Why the login shell failed, or that it was not asked.
        detail: String,
    },

    /// A transaction is pending, so the recorded path stays as it is.
    #[error("a `pohunek service` transaction is pending; the recorded search path is unchanged")]
    RefreshBlockedByTransaction,

    /// `service refresh-path` found no daemon job to replace.
    ///
    /// `effects` is what the run left behind, computed from the state it ended
    /// in; `None` claims nothing (a site that did not compute it).
    ///
    /// Only `refresh-path` returns it, and a refresh never changes the active
    /// version.
    #[error("no daemon job is registered, so the new search path has nothing to take effect in{}", job_missing_effects_text(.effects.as_ref()))]
    DaemonJobMissing {
        /// What the run did to `service.toml` and to the transaction record.
        effects: Option<JobMissingEffects>,
    },

    /// `service.toml` changed while a search-path refresh was pending.
    #[error("service.toml is neither the file the pending search-path refresh started from nor the one it wrote, so it was edited meanwhile; nothing was changed")]
    RefreshConfigChanged,

    /// A search-path refresh is pending and blocks this command.
    #[error(
        "a search-path refresh is pending at step {step}{}{}",
        if *rolling_back { " (rolling back)" } else { "" },
        match restored {
            Some(true) => "; the version directory its daemon runs from was restored",
            Some(false) => "; its version directory is trusted, nothing was changed",
            None => "",
        }
    )]
    RefreshPending {
        /// The step the refresh reached.
        step: &'static str,
        /// Whether its rollback has begun.
        rolling_back: bool,
        /// What `upgrade` did to the version directory the rollback restarts
        /// the daemon from: `Some(true)` restored it, `Some(false)` found it
        /// trusted. `None` when no version directory was examined.
        restored: Option<bool>,
    },

    /// Another `pohunek service` command holds the transaction lock.
    #[error("another `pohunek service` command is running (lock {})", path.display())]
    TransactionInProgress {
        /// The held lock file.
        path: PathBuf,
    },

    /// `pohunek service check --prefix` names another prefix than the
    /// installation an upgrade keeps.
    #[error(
        "--prefix {} differs from the installed prefix {}; `pohunek service upgrade` keeps the installed prefix",
        requested.display(),
        installed.display()
    )]
    PrefixMismatch {
        /// The prefix the caller asked for.
        requested: PathBuf,
        /// The prefix `service.toml` records.
        installed: PathBuf,
    },

    /// `POHUNEK_SERVICE_LOCK_TOKEN` does not prove a live `pohunek service
    /// lock` holder of the transaction lock.
    #[error(
        "{} does not prove a held `pohunek service` transaction lock: {detail}",
        super::inherited::LOCK_TOKEN_ENV
    )]
    InheritedLock {
        /// Why the token was refused.
        detail: String,
    },

    /// A step failed and rolling the transaction back failed too.
    #[error("{original}; rolling the transaction back also failed: {rollback}")]
    RollbackFailed {
        /// The failure that triggered the rollback.
        original: Box<Error>,
        /// The rollback failure; the record is kept for the next run.
        rollback: Box<Error>,
    },

    /// Test-only interruption injected after a journaled step.
    #[cfg(test)]
    #[error("interrupted after step {0:?}")]
    Interrupted(super::record::Step),

    /// A workflow plugin refuses the service uninstall, with the code,
    /// message, recovery and class its `pohunek workflow` error carries.
    #[error("{message}")]
    Workflow {
        /// The stable code of the workflow error.
        code: &'static str,
        /// The message of the workflow error.
        message: String,
        /// The recovery hint of the workflow error.
        hint: &'static str,
        /// The error class of the workflow error.
        class: protocol::ErrorClass,
    },
}

impl From<crate::workflow::Error> for Error {
    fn from(error: crate::workflow::Error) -> Self {
        Self::Workflow {
            code: error.code(),
            message: error.to_string(),
            hint: error.hint(),
            class: error.class(),
        }
    }
}

/// What a `refresh-path` run did to `service.toml`, observed after it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEffect {
    /// The file is byte-for-byte what the run found.
    Untouched,
    /// The run put the previous search path back.
    RestoredToPrevious,
    /// The file records the refresh's new search path.
    NewPath,
}

/// What is left of the refresh's transaction record, observed after the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordEffect {
    /// No record existed before the run and none exists now.
    Absent,
    /// The run removed the record it found or wrote.
    Cleared,
    /// The record stays and the next run continues the rollback.
    KeptRollingBack,
    /// The record stays.
    Kept,
}

/// The effects a missing-job refusal reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobMissingEffects {
    /// What happened to `service.toml`.
    pub file: FileEffect,
    /// What is left of the transaction record.
    pub record: RecordEffect,
    /// Whether the rollback of the run registered the daemon job again from
    /// the previous definition.
    pub job_registered: bool,
}

impl JobMissingEffects {
    fn sentence(self) -> String {
        let file = match self.file {
            FileEffect::Untouched => "service.toml was not changed by this run",
            FileEffect::RestoredToPrevious => {
                "this run put service.toml back on the previous search path"
            }
            FileEffect::NewPath => "service.toml records the new search path",
        };
        let record = match self.record {
            RecordEffect::Absent => "no transaction record exists",
            RecordEffect::Cleared => "the transaction record was cleared",
            RecordEffect::KeptRollingBack => "the transaction record stays rolling_back",
            RecordEffect::Kept => "the transaction record is kept",
        };
        let job = if self.job_registered {
            "; the rollback registered the daemon job again from the previous definition"
        } else {
            ""
        };
        format!("; {file}; {record}{job}")
    }
}

fn job_missing_effects_text(effects: Option<&JobMissingEffects>) -> String {
    effects
        .copied()
        .map(JobMissingEffects::sentence)
        .unwrap_or_default()
}

/// The way to abandon an upgrade an earlier run left pending, which the
/// at-risk refusal of its resume names.
macro_rules! abandon_pending_upgrade {
    () => {
        "; to abandon the pending upgrade instead, roll it back with `pohunek service upgrade --from <prefix>/libexec/pohunek/<previous version>` (`pohunek service status` shows that version as `from`), and the previous daemon's adoption verdict is taken again before it is restored"
    };
}

macro_rules! at_risk_restored_path {
    () => {
        "if the runtime moved since the search path was recorded, do not accept the loss for that cause: with the same `pohunek` that started the pending search-path refresh (the release's `./pohunek` if you used it), run `pohunek service refresh-path` without --dry-run (a dry run is refused while a refresh is pending) to finish it, then rerun the upgrade; otherwise preserve the required runtime or native recovery for the listed sessions, or pass --accept-runtime-loss (to packaging/install-daemon.sh as well) to upgrade anyway and accept the loss of their runtime or native recovery; the accepted risks are recorded so the identical risk is not asked about again, and no session is changed"
    };
}

macro_rules! at_risk_stale_path {
    () => {
        "if the runtime moved since the search path was recorded, do not accept the loss for that cause: with the `pohunek` of the release you are installing (`./pohunek` in its unpacked archive), run `pohunek service refresh-path --dry-run`, then `pohunek service refresh-path` (it restarts the daemon with the new path), then rerun the upgrade; otherwise preserve the required runtime or native recovery for the listed sessions, or pass --accept-runtime-loss (to packaging/install-daemon.sh as well) to upgrade anyway and accept the loss of their runtime or native recovery; the accepted risks are recorded so the identical risk is not asked about again, and no session is changed"
    };
}

macro_rules! at_risk_plain {
    () => {
        "preserve the required runtime or native recovery for the listed sessions, or pass --accept-runtime-loss (to packaging/install-daemon.sh as well) to upgrade anyway and accept the loss of their runtime or native recovery; the accepted risks are recorded so the identical risk is not asked about again, and no session is changed"
    };
}

const AT_RISK_RESTORED_PATH_HINT: &str = at_risk_restored_path!();
const AT_RISK_STALE_PATH_HINT: &str = at_risk_stale_path!();
const AT_RISK_STALE_PATH_RESUMED_HINT: &str =
    concat!(at_risk_stale_path!(), abandon_pending_upgrade!());
const AT_RISK_HINT: &str = at_risk_plain!();
const AT_RISK_RESUMED_HINT: &str = concat!(at_risk_plain!(), abandon_pending_upgrade!());

/// The way out of a refresh blocked by a pending upgrade or install.
const REFRESH_BLOCKED_HINT: &str = "finish the pending transaction first: `pohunek service status` names it, and rerunning the command it names resumes or rolls it back; then rerun `pohunek service refresh-path`. If the resumed upgrade is refused again with the same warning, the record keeps blocking refresh-path: do not accept the runtime loss unless you intend to lose it. To abandon a pending upgrade instead, roll it back with `pohunek service upgrade --from <prefix>/libexec/pohunek/<previous version>` (`pohunek service status` shows that version as `from`); the previous daemon's adoption verdict is taken again before it is restored";

/// The way out of a missing daemon job: `upgrade` over the installed active
/// version registers it again from `service.toml`.
const DAEMON_JOB_MISSING_HINT: &str = "`pohunek service upgrade --from <prefix>/libexec/pohunek/<active version>` registers the daemon job again from service.toml and keeps the settings and the live sessions (use the release archive of that version with `--repair` if its directory is refused); `pohunek service uninstall` removes the installation instead";

/// What a refused refresh leaves when its rollback registered the job again.
const DAEMON_JOB_REGISTERED_AGAIN_HINT: &str = "the rollback registered the daemon job again, so rerun `pohunek service refresh-path` to try the refresh again";

/// The way out of a refused or missing version directory, for the commands
/// that take a release with `--from`.
pub(crate) const UNTRUSTED_VERSION_HINT: &str = "make sure nothing runs from that version directory, then run `pohunek service upgrade --repair --from <unpacked release archive of the version the path names>` (`pohunek service install --repair` while an install is interrupted): it moves the directory aside, publishes the release's copy, and keeps service.toml and the live sessions";

/// The way out of a pending search-path refresh whose daemon replacement or
/// rollback needs a version directory that cannot be trusted.
const UNTRUSTED_REFRESH_VERSION_HINT: &str = "`pohunek service upgrade` and `install` refuse a pending refresh, so restore the directory in two steps: run `pohunek service upgrade --repair --from <unpacked release archive of the version the path names>` (it restores that directory, changes no daemon and no setting, and ends with `service_refresh_pending`), then rerun `pohunek service refresh-path` to finish the refresh or its rollback";

/// The way out of an uninstall refused because the active version directory
/// cannot be trusted while worker jobs are live.
const UNTRUSTED_ACTIVE_VERSION_HINT: &str = "live worker jobs can be stopped only through the daemon, and the active version's daemon is refused: restore its directory with `pohunek service upgrade --repair --from <unpacked release archive of that version>`, then rerun `pohunek service uninstall --stop-sessions`";

impl Error {
    /// Marks an at-risk refusal as that of a resumed upgrade.
    pub(crate) fn resumed(self) -> Self {
        match self {
            Self::UpgradeAtRisk {
                sessions,
                stopped_sessions,
                acceptance,
                warnings,
                ..
            } => Self::UpgradeAtRisk {
                sessions,
                stopped_sessions,
                acceptance,
                warnings,
                resumed: true,
            },
            other => other,
        }
    }

    /// Fills the effects of every missing-job refusal inside `self`.
    pub(crate) fn with_job_missing_effects(self, effects: JobMissingEffects) -> Self {
        match self {
            Self::DaemonJobMissing { .. } => Self::DaemonJobMissing {
                effects: Some(effects),
            },
            Self::RollbackFailed { original, rollback } => Self::RollbackFailed {
                original: Box::new(original.with_job_missing_effects(effects)),
                rollback,
            },
            other => other,
        }
    }

    /// Whether `self` is, or its rollback failure wraps, a refused version
    /// directory.
    fn is_refused_directory(&self) -> bool {
        match self {
            Self::UntrustedVersion { .. }
            | Self::UntrustedActiveVersion { .. }
            | Self::UntrustedRefreshVersion { .. } => true,
            Self::RollbackFailed { rollback, .. } => rollback.is_refused_directory(),
            _ => false,
        }
    }

    /// Whether `self` is, or wraps, a missing-job refusal.
    pub(crate) fn is_job_missing(&self) -> bool {
        match self {
            Self::DaemonJobMissing { .. } => true,
            Self::RollbackFailed { original, .. } => original.is_job_missing(),
            _ => false,
        }
    }

    /// Attaches the warnings of a degraded judgement to a refusal that the
    /// judgement produced, so the operator reads them with the refusal.
    ///
    /// The at-risk refusals keep them in a slot of their own; the preflight
    /// failures append them to their detail. Other errors are returned unchanged.
    #[must_use]
    pub(crate) fn with_warnings(mut self, warnings: &[String]) -> Self {
        match &mut self {
            Self::UpgradeAtRisk { warnings: slot, .. }
            | Self::RefreshAtRisk { warnings: slot, .. }
            | Self::RollbackAtRisk { warnings: slot, .. }
            | Self::RefreshRollbackAtRisk { warnings: slot, .. } => {
                slot.extend_from_slice(warnings);
            }
            Self::UpgradePreflightFailed { detail, .. }
            | Self::RefreshPreflightFailed { detail, .. }
            | Self::RollbackPreflightFailed { detail, .. } => {
                for warning in warnings {
                    detail.push_str("; warning: ");
                    detail.push_str(warning);
                }
            }
            _ => {}
        }
        self
    }

    /// Stable machine-readable code for `--json` error output.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingEnv { .. } => "missing_env",
            Self::Paths(_) => "paths_unavailable",
            Self::NonUtf8Env { .. } => "service_environment_not_utf8",
            Self::UnusableEnv { .. } => "service_environment_invalid",
            Self::InvalidPath { .. } => "cli_usage",
            Self::SearchPath(_) => "service_search_path_unavailable",
            Self::ResumeConfigMismatch { .. } => "service_resume_config_mismatch",
            Self::Config(_) => "service_config_invalid",
            Self::UntrustedDirectory { .. } => "service_untrusted_directory",
            Self::Filesystem { .. } | Self::Io { .. } | Self::CommandSpawn { .. } => {
                "service_io_failed"
            }
            Self::StagedBinary { .. } => "service_staged_binary_missing",
            Self::StagedAnchor { .. } => "service_staged_anchor_invalid",
            Self::StagedRuntime { .. } => "service_staged_runtime_invalid",
            Self::VersionProbe { .. } => "service_version_probe_failed",
            Self::VersionMismatch { .. } => "service_version_mismatch",
            Self::VersionConflict { .. } => "service_version_conflict",
            Self::UntrustedVersion { .. }
            | Self::UntrustedActiveVersion { .. }
            | Self::UntrustedRefreshVersion { .. } => "service_version_untrusted",
            Self::PrefixOwned { .. } => "service_prefix_owned",
            Self::AlreadyInstalled { .. } => "service_already_installed",
            Self::NotInstalled { .. } => "service_not_installed",
            Self::PendingInstall { .. } => "service_install_pending",
            Self::InstallIncomplete { .. } => "service_install_incomplete",
            Self::DaemonJobPresent { .. } => "service_daemon_job_present",
            Self::Supervisor { .. } => "service_supervisor_failed",
            Self::VerifierMissing => "service_verifier_missing",
            Self::UnitVerification { .. } => "service_unit_invalid",
            Self::DaemonNotReady { .. } => "service_daemon_not_ready",
            Self::SupervisedVersion { .. } => "service_daemon_version_unexpected",
            Self::Control { .. } => "service_daemon_request_failed",
            Self::LiveSessions { .. } => "service_live_sessions",
            Self::StopTimeout { .. } => "service_stop_timeout",
            Self::UnreadableJournals { .. } => "service_unreadable_journals",
            Self::OutdatedJournals { .. } => "service_outdated_journals",
            Self::UpgradeAtRisk { .. } | Self::RefreshAtRisk { .. } => {
                "service_upgrade_sessions_at_risk"
            }
            Self::UpgradeStoreUnusable { .. } | Self::RefreshStoreUnusable { .. } => {
                "service_upgrade_store_unusable"
            }
            Self::UpgradePreflightFailed { .. } | Self::RefreshPreflightFailed { .. } => {
                "service_upgrade_preflight_failed"
            }
            Self::UpgradeRuntimesRefused { .. } => "service_upgrade_runtimes_refused",
            Self::UpgradeRuntimesFailed { .. } => "service_upgrade_runtimes_failed",
            Self::UpgradeRuntimesUnconfirmed { .. } => "service_upgrade_runtimes_unconfirmed",
            Self::ManagerEnvironment { .. } => "service_manager_environment_unavailable",
            Self::ManagerVariablesUnread { .. } => "service_manager_variables_unread",
            Self::RuntimeBackup { .. } => "service_runtime_backup_invalid",
            Self::RollbackStoreUnusable { .. } => "service_rollback_store_unusable",
            Self::RollbackAtRisk { .. } | Self::RefreshRollbackAtRisk { .. } => {
                "service_rollback_sessions_at_risk"
            }
            Self::RollbackPreflightFailed { .. } => "service_rollback_preflight_failed",
            Self::OrphanWorkers { .. } => "service_orphan_workers",
            Self::Record { .. } => "service_record_invalid",
            Self::AcceptedRisks { .. } => "service_accepted_risks_invalid",
            Self::ConfigBackup { .. } => "service_config_backup_invalid",
            Self::TransactionInProgress { .. } => "service_transaction_in_progress",
            Self::SearchPathUnmanaged => "service_search_path_unmanaged",
            Self::RefreshConfigSchema { .. } => "service_refresh_config_schema",
            Self::SearchPathRefreshFailed { .. } => "service_search_path_refresh_failed",
            Self::RefreshBlockedByTransaction => "service_refresh_blocked_by_transaction",
            Self::DaemonJobMissing { .. } => "service_daemon_job_missing",
            Self::RefreshConfigChanged => "service_refresh_config_changed",
            Self::RefreshPending { .. } => "service_refresh_pending",
            Self::InheritedLock { .. } => "service_inherited_lock_invalid",
            Self::PrefixMismatch { .. } => "service_prefix_mismatch",
            Self::RollbackFailed { .. } => "service_rollback_failed",
            #[cfg(test)]
            Self::Interrupted(_) => "service_interrupted",
            Self::Workflow { code, .. } => code,
        }
    }

    /// Recovery hint shown beneath the error, when one applies.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per error code, each a single recovery sentence"
    )]
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::AlreadyInstalled { .. } => {
                Some("run `pohunek service upgrade` to install a new version")
            }
            Self::NotInstalled { .. } => Some("run `pohunek service install` first"),
            Self::PendingInstall { .. } => Some(
                "rerun `pohunek service install` (or packaging/install-daemon.sh) with the same version and prefix to finish it, or `pohunek service uninstall` to remove it",
            ),
            Self::InstallIncomplete { .. } => Some(
                "rerun `pohunek service install` (or packaging/install-daemon.sh) with the same version and prefix to finish it, or run `pohunek service uninstall`, which checks for live sessions before removing it",
            ),
            Self::UntrustedVersion { .. } => Some(UNTRUSTED_VERSION_HINT),
            Self::UntrustedActiveVersion { .. } => Some(UNTRUSTED_ACTIVE_VERSION_HINT),
            Self::UntrustedRefreshVersion { .. } => Some(UNTRUSTED_REFRESH_VERSION_HINT),
            Self::PrefixOwned { .. } => Some(
                "pass another --prefix, or uninstall the installation of that namespace first; if it no longer exists, delete the named ownership record",
            ),
            Self::DaemonJobPresent { .. } => {
                Some("remove the stale daemon job with the service manager, then retry")
            }
            Self::LiveSessions { .. } => Some(
                "stop the sessions first, or pass --stop-sessions to stop them as part of uninstall",
            ),
            Self::StopTimeout { .. } => {
                Some("inspect the listed sessions with `pohunek session list` and retry")
            }
            Self::UpgradeAtRisk {
                warnings, resumed, ..
            } => {
                let restored = warnings
                    .iter()
                    .any(|warning| warning.contains(super::preflight::RESTORED_PATH_NOTE));
                let stale = warnings
                    .iter()
                    .any(|warning| warning.starts_with(super::preflight::STALE_PATH_WARNING));
                Some(match (restored, stale, resumed) {
                    (true, _, _) => AT_RISK_RESTORED_PATH_HINT,
                    (false, true, false) => AT_RISK_STALE_PATH_HINT,
                    (false, true, true) => AT_RISK_STALE_PATH_RESUMED_HINT,
                    (false, false, false) => AT_RISK_HINT,
                    (false, false, true) => AT_RISK_RESUMED_HINT,
                })
            }
            Self::RefreshAtRisk { .. } => Some(
                "preserve the required runtime or native recovery for the listed sessions, or run `pohunek service refresh-path --accept-runtime-loss` to restart the daemon with the new search path anyway and accept the loss of the listed sessions' runtime or native recovery; it records nothing, so a later upgrade judges the sessions again, and no session is changed",
            ),
            Self::AcceptedRisks { .. } => Some(
                "remove the ledger file and rerun with --accept-runtime-loss to accept the listed sessions again; the file only records what an earlier upgrade accepted",
            ),
            Self::UpgradeStoreUnusable { .. } => Some(
                "--accept-runtime-loss does not apply: install a pohunek release that reads this store, or restore the store's .pre-schema-<n> backup",
            ),
            Self::RefreshStoreUnusable { .. } => Some(
                "--accept-runtime-loss does not apply: the daemon of the installed release cannot read this store, so restore the store's .pre-schema-<n> backup or install a pohunek release that reads it; nothing was changed",
            ),
            Self::UpgradeRuntimesRefused { .. } => Some(
                "the release's official runtime packages cannot be installed or bound as planned; the listed subjects name what to fix, and nothing was changed",
            ),
            Self::UpgradeRuntimesUnconfirmed { .. } => Some(
                "the upgrade is kept, not rolled back: confirm that no `pohunekd ensure-official-runtimes` process of that release is running (`pgrep -fl ensure-official-runtimes`), then rerun `pohunek service upgrade`",
            ),
            Self::ManagerEnvironment { .. } => Some(
                "the daemon job's environment is judged as its service manager gives it: on Linux the systemd user manager's Environment and the daemon unit with its drop-ins (a drop-in can replace the recorded PATH, and an EnvironmentFile=, PassEnvironment=, UnsetEnvironment= or unreadable line the message names cannot be judged), on macOS the launchd domain and the persisted launchd configuration; make that manager readable, that is its bus reachable and its answer well formed, or remove the unit setting, drop-in or journal ambiguity the message names (for a store variable in an EnvironmentFile=, move it to an Environment= line of a drop-in), and retry; nothing was changed",
            ),
            Self::ManagerVariablesUnread { .. } => Some(
                "log the user in to a GUI session so launchd has the gui/<uid> domain and retry, or pass --accept-runtime-loss to accept that a session judged without the launchd variables may lose its recovery; nothing was changed",
            ),
            Self::UpgradeRuntimesFailed { .. } => Some(
                "run `<staged>/pohunekd ensure-official-runtimes --dry-run` by hand with the same XDG environment to see its output, then retry",
            ),
            Self::RuntimeBackup { .. } => Some(
                "keep the transaction and restore the backed-up runtime state by hand, or rerun the upgrade to take a fresh backup",
            ),
            Self::UpgradePreflightFailed { .. } => Some(
                "run `<staged>/pohunekd upgrade-preflight` by hand with the same XDG environment to see its error, then retry",
            ),
            Self::RefreshPreflightFailed { .. } => Some(
                "run `<prefix>/libexec/pohunek/<active>/pohunekd upgrade-preflight` (the daemon of the installed release, named in the message) by hand with the same XDG environment to see its error, then retry; nothing was changed",
            ),
            Self::RollbackStoreUnusable { .. } => Some(
                "--accept-runtime-loss cannot make the previous daemon read this store; restore its .pre-schema-<n> backup or keep the newer release installed",
            ),
            Self::RollbackAtRisk { .. } => Some(
                "preserve the required runtime or native recovery for the listed sessions, or rerun the upgrade with --accept-runtime-loss to accept their loss during rollback",
            ),
            Self::RefreshRollbackAtRisk { .. } => Some(
                "preserve the required runtime or native recovery for the listed sessions and rerun `pohunek service refresh-path`, or run `pohunek service refresh-path --accept-runtime-loss` to finish the rollback anyway and accept the loss of the listed sessions' runtime or native recovery; it records nothing",
            ),
            Self::RollbackPreflightFailed { .. } => Some(
                "keep the transaction and restore the previous daemon only after its store and runtime compatibility can be proven",
            ),
            Self::VerifierMissing => Some("install systemd's `systemd-analyze` and retry"),
            Self::Config(pohunek_service_config::ConfigError::NamespaceMismatch { .. }) => Some(
                "run the command as the user and with the XDG_STATE_HOME and XDG_RUNTIME_DIR the installation was made with",
            ),
            Self::NonUtf8Env { .. } => {
                Some("point the named variable at a UTF-8 path, then retry")
            }
            Self::Workflow { hint, .. } => Some(hint),
            Self::MissingEnv { var } if var == pohunek_paths::HOME => Some(
                "set HOME to your existing home directory, the session workers' working directory, then retry",
            ),
            Self::SearchPath(_) => Some(
                "create at least one trusted tool directory (for example ~/.local/bin, not writable by group or others) and run `pohunek service install` again",
            ),
            Self::ResumeConfigMismatch { .. } => Some(
                "restore service.toml, or remove it and the interrupted install's record with `pohunek service uninstall`, then install again",
            ),
            Self::UnusableEnv { .. } => Some(
                "point the named variable at an absolute normalized path without `.` or `..` segments (HOME must also be an existing directory), then retry",
            ),
            Self::OutdatedJournals { .. } => Some(
                "end those sessions with the pohunek version that started them, or terminate the listed worker pids, then retry; a journal whose worker pid is unreadable blocks until you delete it after confirming no older pohunek-sessiond runs",
            ),
            Self::VersionMismatch { .. } | Self::StagedBinary { .. } => Some(
                "pass --from with a directory holding pohunek, pohunekd, and pohunek-sessiond of one build",
            ),
            Self::StagedRuntime { .. } => Some(
                "rebuild the release so its runtime/ directory holds only runtime-catalog.json, packages/, and attestations/ regular files, then retry",
            ),
            Self::StagedAnchor { .. } => Some(
                "restore runtime-catalog-anchor.json of the build being installed, or remove it to install a development build without a catalog trust anchor",
            ),
            Self::DaemonNotReady { .. } => {
                Some("inspect the daemon logs, then rerun the command to resume or roll back")
            }
            Self::SupervisedVersion { .. } => Some(
                "inspect which daemon the service job now runs (for example through `systemctl cat` or `launchctl print`), and decide through which pohunek installation's commands it should be stopped or replaced before this command continues",
            ),
            Self::RollbackFailed { rollback, .. } if rollback.is_refused_directory() => {
                rollback.hint()
            }
            Self::RollbackFailed { .. } => {
                Some("rerun the same command; the transaction record resumes the rollback")
            }
            Self::TransactionInProgress { .. } => {
                Some("wait for the other `pohunek service` command to finish, then retry")
            }
            Self::SearchPathUnmanaged => Some(
                "nothing to refresh: this installation takes the PATH of the service manager, which a login-shell discovery does not change",
            ),
            Self::RefreshConfigSchema { found, current } if *found > i64::from(*current) => Some(
                "service.toml was written by a newer release: run `pohunek service refresh-path` of the installed release (`<prefix>/bin/pohunek`); this binary never rewrites it",
            ),
            Self::RefreshConfigSchema { .. } => Some(
                "refresh-path never rewrites a service.toml of an older schema. If the installed release has the command (v0.39 and later), run its own `<prefix>/bin/pohunek service refresh-path`. For a release without it: stop the live sessions, save a copy of service.toml, run `pohunek service uninstall` (it keeps durable metadata and refuses while sessions are live), install the version that is installed now from its own archive with the same --prefix (a fresh install discovers the current path), reapply the non-default settings from the saved copy except `search_path` under [environment] (restoring it brings the stale path back), and rerun the upgrade",
            ),
            Self::SearchPathRefreshFailed { .. } => Some(
                "fix the login shell (a shell rc file that hangs or prints junk is the usual cause) and rerun `pohunek service refresh-path`",
            ),
            Self::RefreshBlockedByTransaction => Some(REFRESH_BLOCKED_HINT),
            Self::DaemonJobMissing {
                effects: Some(JobMissingEffects {
                    job_registered: true,
                    ..
                }),
            } => Some(DAEMON_JOB_REGISTERED_AGAIN_HINT),
            Self::DaemonJobMissing { .. } => Some(DAEMON_JOB_MISSING_HINT),

            Self::RefreshConfigChanged => Some(
                "service.toml was edited while a search-path refresh was pending. Restore it as the refresh wrote it or as it was before the refresh, then rerun `pohunek service refresh-path`; or run `pohunek service uninstall`, which is session-checked and does not need the file to match the refresh",
            ),
            Self::RefreshPending {
                rolling_back: true, ..
            } => Some(
                "rerun `pohunek service refresh-path` without --dry-run (a dry run is refused while a refresh is pending) and with the same `pohunek` (the release's `./pohunek` if you used it); it finishes the rollback after judging the live sessions again (a missing daemon job is registered again by that rerun), and if it refuses with the listed sessions, run `pohunek service refresh-path --accept-runtime-loss` to accept the loss of their runtime or native recovery (it records nothing)",
            ),
            Self::RefreshPending { .. } => Some(
                "rerun `pohunek service refresh-path` without --dry-run (a dry run is refused while a refresh is pending) and with the same `pohunek` (the release's `./pohunek` if you used it); it resumes or rolls back the pending refresh",
            ),
            Self::Record { detail, .. } if detail.contains("is unsupported; expected") => Some(
                "the record was written by a pohunek release with another record schema: rerun the command with the `pohunek` that started the transaction (the `./pohunek` of the release's unpacked archive if you used it, or `<prefix>/bin/pohunek` of the installed release)",
            ),
            Self::PrefixMismatch { .. } => {
                Some("omit --prefix, or pass the installed prefix, to check the upgrade")
            }
            Self::InheritedLock { .. } => Some(
                "run the command under `pohunek service lock -- <command>`, or without POHUNEK_SERVICE_LOCK_TOKEN in its environment",
            ),
            Self::ConfigBackup { .. } => Some(
                "restore the pre-upgrade service.toml and its backup to the exact bytes the transaction record digests, then rerun; or remove the installation with `pohunek service uninstall`, which checks for live sessions first",
            ),
            _ => None,
        }
    }
}

/// The refusal text: one line per session with its verdict, code and evidence.
/// Lists each runtime refusal as `subject: code (detail)`, separated by `; `.
fn refusals_summary(refusals: &[Refusal]) -> String {
    refusals
        .iter()
        .map(|refusal| format!("{}: {} ({})", refusal.subject, refusal.code, refusal.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

fn at_risk_summary(
    action: &str,
    sessions: &[SessionVerdict],
    stopped_sessions: &[SessionVerdict],
    acceptance: &AcceptanceGroups,
    warnings: &[String],
) -> String {
    let counted: Vec<String> = [
        ("live", sessions.len()),
        ("stopped", stopped_sessions.len()),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(kind, count)| format!("{count} {kind} session(s)"))
    .collect();
    let mut text = format!(
        "{action} refused: {} have unverified runtime or recovery",
        counted.join(", ")
    );
    for (kind, session) in sessions
        .iter()
        .map(|session| ("live", session))
        .chain(stopped_sessions.iter().map(|session| ("stopped", session)))
    {
        let name = session
            .name
            .as_deref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let _ = write!(
            text,
            "\n  {kind} {}{name}: {} [{}] {}",
            session.session_id, session.verdict, session.code, session.detail
        );
    }
    if let Some(version) = &acceptance.previously_accepted_at {
        let _ = write!(
            text,
            "\n  previously accepted at upgrade to {version} (not blocking): {} session(s)",
            acceptance.previously_accepted.len()
        );
        for session in &acceptance.previously_accepted {
            let _ = write!(
                text,
                "\n    {}: {} [{}]",
                session.session_id, session.verdict, session.code
            );
        }
    }
    for warning in warnings {
        let _ = write!(text, "\n  warning: {warning}");
    }
    text
}

fn live_summary(prefix: &str, sessions: &[LiveSession], workers: &[String]) -> String {
    let mut parts = Vec::new();
    if !sessions.is_empty() {
        parts.push(format!(
            "sessions: {}",
            sessions
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !workers.is_empty() {
        parts.push(format!("worker jobs: {}", workers.join(", ")));
    }
    format!("{prefix}; {}", parts.join("; "))
}

/// Group and other write bits.
///
/// [`FsError::UnsafeMode`] comes from two rules: the forbidden-bits rule,
/// which fails exactly when one of these bits is set on the entry
/// (remediated by `chmod go-w`), and the exact-mode rule, which fails
/// whenever the entry's mode is not exactly the required owner-private mode
/// (remediated by `chmod go-rwx`). Their presence therefore discriminates
/// the two causes so each gets a remediation that actually resolves it.
const GROUP_OR_OTHER_WRITE_BITS: u32 = 0o022;

/// Builds the remediation for an entry that lets other users write or that
/// may be owned by someone else: the `chmod go-w` hint plus the ownership
/// alternative.
fn writable_fix(path: &Path) -> String {
    format!(
        "run `chmod go-w {}` (or fix its owner) and retry",
        path.display()
    )
}

/// Maps both rules that raise [`FsError::UnsafeMode`] to a message whose
/// remediation resolves the rule that failed: `chmod go-w` for forbidden
/// group/other write bits, `chmod go-rwx` for an exact-mode mismatch such
/// as a `0700` state directory found at `0755`.
fn unsafe_mode_error(path: PathBuf, actual: u32, expected: u32) -> Error {
    if actual & GROUP_OR_OTHER_WRITE_BITS != 0 {
        Error::UntrustedDirectory {
            detail: format!("mode {actual:#o} lets other users write to it"),
            fix: writable_fix(&path),
            path,
        }
    } else {
        Error::UntrustedDirectory {
            detail: format!("must be exactly {expected:#o}, found {actual:#o}"),
            fix: format!("run `chmod go-rwx {}` and retry", path.display()),
            path,
        }
    }
}

/// Maps a trusted-filesystem failure, naming an unsafe directory precisely.
pub(crate) fn fs_error(operation: &'static str, source: FsError) -> Error {
    match source {
        FsError::UnsafeMode {
            path,
            actual,
            expected,
        } => unsafe_mode_error(path, actual, expected),
        FsError::UnsafeOwner { path, actual, .. } => Error::UntrustedDirectory {
            detail: format!("it is owned by uid {actual}"),
            fix: writable_fix(&path),
            path,
        },
        FsError::UnsafeAcl { path } => Error::UntrustedDirectory {
            detail: "an extended ACL grants other users access".to_owned(),
            fix: writable_fix(&path),
            path,
        },
        source => Error::Filesystem { operation, source },
    }
}

/// Maps an atomic-replace failure.
pub(crate) fn replace_error(
    operation: &'static str,
    source: pohunek_platform::filesystem::AtomicReplaceError,
) -> Error {
    use pohunek_platform::filesystem::AtomicReplaceError;

    match source {
        AtomicReplaceError::BeforeCommit(source)
        | AtomicReplaceError::CommittedDurabilityUncertain(source) => fs_error(operation, source),
        other => Error::Io {
            operation,
            path: std::path::PathBuf::new(),
            source: io::Error::other(other.to_string()),
        },
    }
}

/// Maps a backend failure, surfacing an untrusted unit or agent directory.
pub(crate) fn supervisor_error(operation: &'static str, source: supervisor::Error) -> Error {
    if let supervisor::Error::Operation { source: inner, .. } = &source {
        if let Some(fs) = inner.downcast_ref::<FsError>() {
            match fs {
                FsError::UnsafeMode {
                    path,
                    actual,
                    expected,
                } => {
                    return unsafe_mode_error(path.clone(), *actual, *expected);
                }
                FsError::UnsafeOwner { path, actual, .. } => {
                    return Error::UntrustedDirectory {
                        detail: format!("it is owned by uid {actual}"),
                        fix: writable_fix(path),
                        path: path.clone(),
                    };
                }
                _ => {}
            }
        }
    }
    Error::Supervisor { operation, source }
}

/// Maps a plain I/O failure on `path`.
pub(crate) fn io_error(
    operation: &'static str,
    path: impl Into<PathBuf>,
) -> impl FnOnce(io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io {
        operation,
        path,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_directory_names_the_path_and_the_fix() {
        let error = fs_error(
            "open unit directory",
            FsError::UnsafeMode {
                path: PathBuf::from("/home/u/.config"),
                actual: 0o40777,
                expected: 0o700,
            },
        );
        let text = error.to_string();
        assert!(text.contains("/home/u/.config"), "{text}");
        assert!(
            text.contains("mode 0o40777 lets other users write to it"),
            "{text}"
        );
        assert!(text.contains("chmod go-w /home/u/.config"), "{text}");
        assert_eq!(error.code(), "service_untrusted_directory");
    }

    #[test]
    fn an_exact_mode_mismatch_names_the_required_mode_and_a_working_fix() {
        let error = fs_error(
            "open state directory",
            FsError::UnsafeMode {
                path: PathBuf::from("/home/u/.local/state/pohunek"),
                actual: 0o755,
                expected: 0o700,
            },
        );
        let text = error.to_string();
        assert!(text.contains("/home/u/.local/state/pohunek"), "{text}");
        assert!(
            text.contains("must be exactly 0o700, found 0o755"),
            "{text}"
        );
        assert!(!text.contains("lets other users write"), "{text}");
        // The remediation is the command between the backticks; it must be a
        // plain `chmod` invocation the operator can run verbatim.
        let fix = text.split('`').nth(1).expect("fix command in backticks");
        assert_eq!(fix, "chmod go-rwx /home/u/.local/state/pohunek");
        assert_eq!(error.code(), "service_untrusted_directory");
    }

    #[test]
    fn an_exact_mode_mismatch_is_recognized_inside_backend_errors() {
        let error = supervisor_error(
            "install",
            supervisor::Error::Operation {
                operation: "open_state_directory",
                source: Box::new(FsError::UnsafeMode {
                    path: PathBuf::from("/home/u/.local/state/pohunek"),
                    actual: 0o755,
                    expected: 0o700,
                }),
            },
        );
        let Error::UntrustedDirectory { detail, fix, .. } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(detail, "must be exactly 0o700, found 0o755");
        assert_eq!(
            fix,
            "run `chmod go-rwx /home/u/.local/state/pohunek` and retry"
        );
    }

    #[test]
    fn untrusted_unit_directory_is_recognized_inside_backend_errors() {
        let error = supervisor_error(
            "install",
            supervisor::Error::Operation {
                operation: "open_unit_directory",
                source: Box::new(FsError::UnsafeMode {
                    path: PathBuf::from("/home/u/.config/systemd"),
                    actual: 0o777,
                    expected: 0o755,
                }),
            },
        );
        assert!(
            matches!(&error, Error::UntrustedDirectory { path, .. } if path == &PathBuf::from("/home/u/.config/systemd")),
            "{error:?}"
        );
    }

    #[test]
    fn live_sessions_list_ids_and_names() {
        let error = Error::LiveSessions {
            sessions: vec![
                LiveSession {
                    id: "s-1".to_owned(),
                    name: Some("review".to_owned()),
                },
                LiveSession {
                    id: "s-2".to_owned(),
                    name: None,
                },
            ],
            workers: vec!["s-3.abcd2345".to_owned()],
        };
        assert_eq!(
            error.to_string(),
            "uninstall refused: live sessions remain; sessions: s-1 (review), s-2; \
             worker jobs: s-3.abcd2345"
        );
        assert_eq!(error.code(), "service_live_sessions");
        assert!(error
            .hint()
            .is_some_and(|hint| hint.contains("--stop-sessions")));
    }
}
