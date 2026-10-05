//! Typed failures of `pohunek service`.

// Rust guideline compliant 2026-09-30

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::FsError;
use pohunek_platform::supervisor;
use pohunek_service_config::preflight::SessionVerdict;

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

    /// A staged binary is missing or not a regular file.
    #[error("staged binary {} is missing or not a regular file", path.display())]
    StagedBinary {
        /// The expected staged binary.
        path: PathBuf,
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

    /// A control request to the local daemon failed.
    #[error("daemon request `{operation}` failed: {source}")]
    Control {
        /// The request.
        operation: &'static str,
        /// The client failure.
        #[source]
        source: pohunek_client::ClientError,
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
    /// recovery, live sessions of the running daemon.
    #[error("{}", at_risk_summary(sessions))]
    UpgradeAtRisk {
        /// The sessions the preflight judged at risk, ordered by id.
        sessions: Vec<SessionVerdict>,
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
}

impl Error {
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
            Self::Filesystem { .. } | Self::Io { .. } => "service_io_failed",
            Self::StagedBinary { .. } => "service_staged_binary_missing",
            Self::VersionProbe { .. } => "service_version_probe_failed",
            Self::VersionMismatch { .. } => "service_version_mismatch",
            Self::VersionConflict { .. } => "service_version_conflict",
            Self::UntrustedVersion { .. } => "service_version_untrusted",
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
            Self::Control { .. } => "service_daemon_request_failed",
            Self::LiveSessions { .. } => "service_live_sessions",
            Self::StopTimeout { .. } => "service_stop_timeout",
            Self::UnreadableJournals { .. } => "service_unreadable_journals",
            Self::OutdatedJournals { .. } => "service_outdated_journals",
            Self::UpgradeAtRisk { .. } => "service_upgrade_sessions_at_risk",
            Self::UpgradeStoreUnusable { .. } => "service_upgrade_store_unusable",
            Self::UpgradePreflightFailed { .. } => "service_upgrade_preflight_failed",
            Self::OrphanWorkers { .. } => "service_orphan_workers",
            Self::Record { .. } => "service_record_invalid",
            Self::TransactionInProgress { .. } => "service_transaction_in_progress",
            Self::InheritedLock { .. } => "service_inherited_lock_invalid",
            Self::PrefixMismatch { .. } => "service_prefix_mismatch",
            Self::RollbackFailed { .. } => "service_rollback_failed",
            #[cfg(test)]
            Self::Interrupted(_) => "service_interrupted",
        }
    }

    /// Recovery hint shown beneath the error, when one applies.
    #[must_use]
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
            Self::UntrustedVersion { .. } => Some(
                "make sure nothing runs from that version directory, remove it, and rerun the command to install it again",
            ),
            Self::PrefixOwned { .. } => Some(
                "pass another --prefix, or uninstall the installation of that namespace first; if it no longer exists, delete the named ownership record",
            ),
            Self::DaemonJobPresent { .. } => Some(
                "remove the stale daemon job with the service manager, then retry",
            ),
            Self::LiveSessions { .. } => Some(
                "stop the sessions first, or pass --stop-sessions to stop them as part of uninstall",
            ),
            Self::StopTimeout { .. } => {
                Some("inspect the listed sessions with `pohunek session list` and retry")
            }
            Self::UpgradeAtRisk { .. } => Some(
                "end or save the listed sessions first, or pass --accept-runtime-loss (to packaging/install-daemon.sh as well) to upgrade anyway and accept the loss of their runtime or native recovery",
            ),
            Self::UpgradeStoreUnusable { .. } => Some(
                "--accept-runtime-loss does not apply: install a pohunek release that reads this store, or restore the store's .pre-schema-<n> backup",
            ),
            Self::UpgradePreflightFailed { .. } => Some(
                "run `<staged>/pohunekd upgrade-preflight` by hand with the same XDG environment to see its error, then retry",
            ),
            Self::VerifierMissing => Some("install systemd's `systemd-analyze` and retry"),
            Self::Config(pohunek_service_config::ConfigError::NamespaceMismatch { .. }) => Some(
                "run the command as the user and with the XDG_STATE_HOME and XDG_RUNTIME_DIR the installation was made with",
            ),
            Self::NonUtf8Env { .. } => {
                Some("point the named variable at a UTF-8 path, then retry")
            }
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
            Self::DaemonNotReady { .. } => {
                Some("inspect the daemon logs, then rerun the command to resume or roll back")
            }
            Self::RollbackFailed { .. } => {
                Some("rerun the same command; the transaction record resumes the rollback")
            }
            Self::TransactionInProgress { .. } => {
                Some("wait for the other `pohunek service` command to finish, then retry")
            }
            Self::PrefixMismatch { .. } => {
                Some("omit --prefix, or pass the installed prefix, to check the upgrade")
            }
            Self::InheritedLock { .. } => Some(
                "run the command under `pohunek service lock -- <command>`, or without POHUNEK_SERVICE_LOCK_TOKEN in its environment",
            ),
            _ => None,
        }
    }
}

/// The refusal text: one line per session with its verdict, code and evidence.
fn at_risk_summary(sessions: &[SessionVerdict]) -> String {
    let mut text = format!(
        "upgrade refused: {} live session(s) would not survive the new daemon intact",
        sessions.len()
    );
    for session in sessions {
        let name = session
            .name
            .as_deref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let _ = write!(
            text,
            "\n  {}{name}: {} [{}] {}",
            session.session_id, session.verdict, session.code, session.detail
        );
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
