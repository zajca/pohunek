//! The adoption preflight of an upgrade, and the operator's choice on its result.
//!
//! The new release's `pohunekd upgrade-preflight` judges, from the installed
//! state on disk, whether it would adopt every live session of the running
//! daemon (see `pohunek_daemon::session::upgrade_preflight` for its evidence).
//! This module runs it, reads the report, and decides: a store the new daemon
//! cannot start with is always refused, and sessions at risk are refused unless
//! the operator passed `--accept-runtime-loss`.

// Rust guideline compliant 2026-06-26

use std::fmt::Debug;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;

use pohunek_paths::DAEMON_EXECUTABLE_NAME;
use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::SEARCH_PATH_VARIABLE;
use pohunek_service_config::preflight::{PreflightReport, SessionVerdict, REPORT_VERSION};
use tokio::io::AsyncReadExt as _;

use super::context::Context;
use super::error::Error;
use super::layout;
use super::runtimes;
use super::settings;

/// Argument that makes the daemon print its preflight report.
pub const PREFLIGHT_ARGUMENT: &str = "upgrade-preflight";

/// A pending preflight run.
pub type Judging<'a> = Pin<Box<dyn Future<Output = Result<PreflightReport, Error>> + Send + 'a>>;

/// Judges the adoption of the live sessions of the installed state.
pub trait AdoptionPreflight: Debug + Send + Sync {
    /// Returns the report of the daemon executable `daemon` for the state
    /// `context` locates, run with the `PATH` `search_path` gives the daemon
    /// job.
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        daemon: &'a Path,
        search_path: &'a SearchPath,
    ) -> Judging<'a>;
}

/// Runs `<daemon> upgrade-preflight` as a child process.
#[derive(Debug, Clone, Copy, Default)]
pub struct DaemonPreflight;

impl AdoptionPreflight for DaemonPreflight {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        daemon: &'a Path,
        search_path: &'a SearchPath,
    ) -> Judging<'a> {
        Box::pin(run_daemon(context, daemon, search_path))
    }
}

/// Sets the `PATH` of a child that stands in for the daemon job.
///
/// An empty search path sets nothing: the job definition carries no `PATH`
/// then, and the service manager's own environment applies.
pub(super) fn set_search_path(command: &mut tokio::process::Command, search_path: &SearchPath) {
    if !search_path.is_empty() {
        command.env(SEARCH_PATH_VARIABLE, search_path.to_env_value());
    }
}

/// The outcome of a preflight the operator may proceed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gated {
    /// What the new daemon reported.
    pub report: PreflightReport,
    /// Whether at-risk sessions exist and `--accept-runtime-loss` accepted them.
    pub accepted: bool,
}

/// Runs the preflight of the staged `daemon` and applies the operator's choice.
///
/// The daemon is first checked to be a regular file that reports `version`, so
/// the binary that judges is of the release being installed.
///
/// # Errors
///
/// Returns [`Error::StagedBinary`] for a missing daemon, the probe errors of
/// [`layout::probe`], [`Error::UpgradePreflightFailed`] when no report comes
/// back, and the refusals of [`decide`].
pub async fn gate(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &Path,
    version: &str,
    search_path: &SearchPath,
    accept_runtime_loss: bool,
) -> Result<Gated, Error> {
    let report = judge_daemon(preflight, context, daemon_dir, version, search_path).await?;
    decide(report, accept_runtime_loss)
}

/// Runs [`gate`] for an upgrade, which also refuses a runtime plan that cannot
/// be applied.
///
/// A rollback never runs the runtime step, so only the upgrade path calls this.
/// The plan refusal is checked before the operator's choice, so
/// `--accept-runtime-loss` cannot accept it.
///
/// # Errors
///
/// Returns [`Error::UpgradeRuntimesRefused`] for a refused plan, and the errors
/// of [`gate`] otherwise.
pub async fn gate_upgrade(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &Path,
    version: &str,
    search_path: &SearchPath,
    accept_runtime_loss: bool,
) -> Result<Gated, Error> {
    let report = judge_daemon(preflight, context, daemon_dir, version, search_path).await?;
    if let Some(plan) = &report.runtimes {
        runtimes::check_plan(plan)?;
    }
    decide(report, accept_runtime_loss)
}

/// Checks the staged `daemon` is of `version` and returns its report.
async fn judge_daemon(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &Path,
    version: &str,
    search_path: &SearchPath,
) -> Result<PreflightReport, Error> {
    let daemon = daemon_dir.join(DAEMON_EXECUTABLE_NAME);
    probe_daemon(&daemon, version).await?;
    preflight.judge(context, &daemon, search_path).await
}

/// Verifies the daemon binary before using it as compatibility evidence.
///
/// # Errors
///
/// Returns the staged-binary or version-probe failure.
pub(super) async fn probe_daemon(daemon: &Path, version: &str) -> Result<(), Error> {
    match std::fs::metadata(daemon) {
        Ok(metadata) if metadata.is_file() => {}
        _ => {
            return Err(Error::StagedBinary {
                path: daemon.to_path_buf(),
            })
        }
    }
    layout::probe(daemon, DAEMON_EXECUTABLE_NAME, version).await?;
    Ok(())
}

/// Applies the operator's choice to `report`.
///
/// # Errors
///
/// Returns [`Error::UpgradeStoreUnusable`] when the new daemon would refuse to
/// start with the store, whatever the flag says, and [`Error::UpgradeAtRisk`]
/// when sessions are at risk and `accept_runtime_loss` is `false`.
pub fn decide(report: PreflightReport, accept_runtime_loss: bool) -> Result<Gated, Error> {
    if report.store_refused() {
        return Err(Error::UpgradeStoreUnusable {
            path: report.store.path,
            code: report.store.error_code.unwrap_or_default(),
            detail: report.store.error.unwrap_or_default(),
        });
    }
    let at_risk: Vec<SessionVerdict> = report.at_risk().cloned().collect();
    if at_risk.is_empty() {
        return Ok(Gated {
            report,
            accepted: false,
        });
    }
    if accept_runtime_loss {
        Ok(Gated {
            report,
            accepted: true,
        })
    } else {
        Err(Error::UpgradeAtRisk { sessions: at_risk })
    }
}

/// Runs the daemon's preflight with the environment the installed daemon gets.
async fn run_daemon(
    context: &Context,
    daemon: &Path,
    search_path: &SearchPath,
) -> Result<PreflightReport, Error> {
    let failure = |detail: String| Error::UpgradePreflightFailed {
        binary: daemon.to_path_buf(),
        detail,
    };
    let mut command = tokio::process::Command::new(daemon);
    command
        .arg(PREFLIGHT_ARGUMENT)
        .env_clear()
        .envs(context.bootstrap_environment()?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    set_search_path(&mut command, search_path);
    let mut child =
        layout::retry_while_exec_busy(settings::EXEC_BUSY_WAIT, settings::EXEC_BUSY_POLL, || {
            command.spawn()
        })
        .await
        .map_err(|error| failure(format!("cannot run it: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout was configured as a pipe");
    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as a pipe");
    let run = async {
        let (report, diagnostics) = tokio::join!(
            read_bounded(stdout, settings::PREFLIGHT_OUTPUT),
            read_bounded(stderr, settings::PREFLIGHT_DIAGNOSTICS),
        );
        let status = child.wait().await;
        (report, diagnostics, status)
    };
    let (report, diagnostics, status) = tokio::time::timeout(settings::PREFLIGHT_TIMEOUT, run)
        .await
        .map_err(|_elapsed| {
            failure(format!(
                "it did not finish within {} s",
                settings::PREFLIGHT_TIMEOUT.as_secs()
            ))
        })?;
    let status = status.map_err(|error| failure(format!("cannot wait for it: {error}")))?;
    if !status.success() {
        let diagnostics = String::from_utf8_lossy(&diagnostics.unwrap_or_default())
            .trim()
            .to_owned();
        return Err(failure(format!("it exited with {status}: {diagnostics}")));
    }
    let report = report.map_err(|error| failure(format!("cannot read its report: {error}")))?;
    let report: PreflightReport = serde_json::from_slice(&report)
        .map_err(|error| failure(format!("its report is not valid: {error}")))?;
    if report.report_version != REPORT_VERSION {
        return Err(failure(format!(
            "its report has version {}, expected {REPORT_VERSION}",
            report.report_version
        )));
    }
    Ok(report)
}

/// Keeps at most `limit` bytes of `reader` and discards the rest.
///
/// A report cut at the limit does not parse, which fails the preflight; cut
/// diagnostics are only context.
pub(super) async fn read_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX);
    (&mut reader).take(cap).read_to_end(&mut bytes).await?;
    // Drain the rest so the child never blocks on a full pipe.
    tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
    Ok(bytes)
}
