//! Typed failures of `pohunek service`.

// Rust guideline compliant 2026-09-24

use std::io;
use std::path::PathBuf;

use pohunek_platform::filesystem::FsError;
use pohunek_platform::supervisor;

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

    /// `--prefix` or `--from` is not an absolute normalized path.
    #[error("{flag} must be an absolute normalized path: {}", path.display())]
    InvalidPath {
        /// The flag carrying the path.
        flag: &'static str,
        /// The rejected path.
        path: PathBuf,
    },

    /// Reading or writing `service.toml` failed.
    #[error(transparent)]
    Config(#[from] pohunek_service_config::ConfigError),

    /// A directory the installer writes into is writable by other users.
    #[error(
        "{} is not a trusted directory: {detail}; run `chmod go-w {}` (or fix its owner) and retry",
        path.display(),
        path.display()
    )]
    UntrustedDirectory {
        /// The directory that failed the ownership or mode check.
        path: PathBuf,
        /// What exactly was wrong with it.
        detail: String,
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

    /// `service upgrade` found an interrupted install transaction.
    ///
    /// Only `service install` finishes or rolls back an install; an upgrade
    /// rolling it back would remove a daemon the install may already run.
    #[error(
        "an interrupted install of version {version} is pending (last completed step: {step}); \
         upgrade refuses to touch it"
    )]
    PendingInstall {
        /// The version the pending install targets.
        version: String,
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
            Self::InvalidPath { .. } => "cli_usage",
            Self::Config(_) => "service_config_invalid",
            Self::UntrustedDirectory { .. } => "service_untrusted_directory",
            Self::Filesystem { .. } | Self::Io { .. } => "service_io_failed",
            Self::StagedBinary { .. } => "service_staged_binary_missing",
            Self::VersionProbe { .. } => "service_version_probe_failed",
            Self::VersionMismatch { .. } => "service_version_mismatch",
            Self::VersionConflict { .. } => "service_version_conflict",
            Self::AlreadyInstalled { .. } => "service_already_installed",
            Self::NotInstalled { .. } => "service_not_installed",
            Self::PendingInstall { .. } => "service_install_pending",
            Self::DaemonJobPresent { .. } => "service_daemon_job_present",
            Self::Supervisor { .. } => "service_supervisor_failed",
            Self::VerifierMissing => "service_verifier_missing",
            Self::UnitVerification { .. } => "service_unit_invalid",
            Self::DaemonNotReady { .. } => "service_daemon_not_ready",
            Self::Control { .. } => "service_daemon_request_failed",
            Self::LiveSessions { .. } => "service_live_sessions",
            Self::StopTimeout { .. } => "service_stop_timeout",
            Self::UnreadableJournals { .. } => "service_unreadable_journals",
            Self::OrphanWorkers { .. } => "service_orphan_workers",
            Self::Record { .. } => "service_record_invalid",
            Self::TransactionInProgress { .. } => "service_transaction_in_progress",
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
                "rerun `pohunek service install` (or packaging/install-daemon.sh) with the same version and prefix to finish it",
            ),
            Self::DaemonJobPresent { .. } => Some(
                "remove the stale daemon job with the service manager, then install again",
            ),
            Self::LiveSessions { .. } => Some(
                "stop the sessions first, or pass --stop-sessions to stop them as part of uninstall",
            ),
            Self::StopTimeout { .. } => {
                Some("inspect the listed sessions with `pohunek session list` and retry")
            }
            Self::VerifierMissing => Some("install systemd's `systemd-analyze` and retry"),
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
            _ => None,
        }
    }
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

/// Maps a trusted-filesystem failure, naming an unsafe directory precisely.
pub(crate) fn fs_error(operation: &'static str, source: FsError) -> Error {
    match source {
        FsError::UnsafeMode { path, actual, .. } => Error::UntrustedDirectory {
            path,
            detail: format!("mode {actual:#o} lets other users write to it"),
        },
        FsError::UnsafeOwner { path, actual, .. } => Error::UntrustedDirectory {
            path,
            detail: format!("it is owned by uid {actual}"),
        },
        FsError::UnsafeAcl { path } => Error::UntrustedDirectory {
            path,
            detail: "an extended ACL grants other users access".to_owned(),
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
                FsError::UnsafeMode { path, actual, .. } => {
                    return Error::UntrustedDirectory {
                        path: path.clone(),
                        detail: format!("mode {actual:#o} lets other users write to it"),
                    };
                }
                FsError::UnsafeOwner { path, actual, .. } => {
                    return Error::UntrustedDirectory {
                        path: path.clone(),
                        detail: format!("it is owned by uid {actual}"),
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
        assert!(text.contains("chmod go-w /home/u/.config"), "{text}");
        assert_eq!(error.code(), "service_untrusted_directory");
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
