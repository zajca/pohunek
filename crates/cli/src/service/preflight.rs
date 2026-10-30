//! The adoption preflight of an upgrade, and the operator's choice on its result.
//!
//! The new release's `pohunekd upgrade-preflight` judges, from the installed
//! state on disk, whether it would adopt every live session of the running
//! daemon (see `pohunek_daemon::session::upgrade_preflight` for its evidence).
//! This module runs it, reads the report, and decides: a store the new daemon
//! cannot start with is always refused, and sessions at risk are refused unless
//! the operator passed `--accept-runtime-loss` or an earlier upgrade accepted
//! the identical risk (see [`super::ledger`]).

// Rust guideline compliant 2026-06-26

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use pohunek_paths::DAEMON_EXECUTABLE_NAME;
use pohunek_platform::supervisor::SEARCH_PATH_VARIABLE;
use pohunek_service_config::preflight::{
    preflight_timeout, PreflightReport, PROBE_BUDGET, PROBE_BUDGET_FLAG, RELEASE_PLAN_FLAG,
    REPORT_VERSION,
};
use tokio::io::AsyncReadExt as _;

use super::context::Context;
use super::error::Error;
use super::layout::{self, VerifiedVersionDir};
use super::ledger::{Acceptance, AcceptedSet, Entry, List, OneTime};
use super::report::AcceptanceGroups;
use super::runtimes;
use super::settings;
use super::VERSION;

/// Argument that makes the daemon print its preflight report.
pub const PREFLIGHT_ARGUMENT: &str = "upgrade-preflight";

/// Whether a preflight judgement includes the release's runtime plan.
///
/// Only the upgrade's own gates include it: the plan is what the upgrade is
/// about to apply. The post-apply judgement runs after the apply, so the
/// installed packages already are the plan's result. A rollback runs the
/// previous release's binary, which never applies a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasePlan {
    /// The judgement serves the installed packages only.
    Omitted,
    /// The judgement adds the release's runtime plan.
    Included,
}

impl ReleasePlan {
    /// The daemon argument this choice passes, if any.
    fn argument(self) -> Option<&'static str> {
        match self {
            Self::Omitted => None,
            Self::Included => Some(RELEASE_PLAN_FLAG),
        }
    }
}

/// A pending preflight run.
pub type Judging<'a> = Pin<Box<dyn Future<Output = Result<PreflightReport, Error>> + Send + 'a>>;

/// Judges the adoption of the live sessions of the installed state.
pub trait AdoptionPreflight: Debug + Send + Sync {
    /// Returns the report of the daemon executable in `dir` for the state
    /// `context` locates, run with the `PATH` `search_path` gives the daemon
    /// job and the service manager's allowlisted `variables`. `probe_budget`
    /// is passed to the daemon only when it is `Some`.
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        search_path: &'a str,
        variables: &'a BTreeMap<String, String>,
        probe_budget: Option<Duration>,
        plan: ReleasePlan,
    ) -> Judging<'a>;
}

/// Runs `<daemon> upgrade-preflight` as a child process.
#[derive(Debug, Clone, Copy, Default)]
pub struct DaemonPreflight;

impl AdoptionPreflight for DaemonPreflight {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        search_path: &'a str,
        variables: &'a BTreeMap<String, String>,
        probe_budget: Option<Duration>,
        plan: ReleasePlan,
    ) -> Judging<'a> {
        Box::pin(run_daemon(
            context,
            dir.daemon(),
            search_path,
            variables,
            probe_budget,
            plan,
        ))
    }
}

/// Sets the `PATH` of a child that stands in for the daemon job, byte for byte.
///
/// The job's `PATH` value is the one the child gets, so the child resolves and
/// probes exactly as the job does.
pub(super) fn set_search_path(command: &mut tokio::process::Command, search_path: &str) {
    command.env(SEARCH_PATH_VARIABLE, search_path);
}

/// The outcome of a preflight the operator may proceed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gated {
    /// What the new daemon reported.
    pub report: PreflightReport,
    /// Whether at-risk sessions that no earlier upgrade accepted exist and
    /// `--accept-runtime-loss` accepted them.
    pub accepted: bool,
    /// Conditions of the judgement's inputs that the operator must be told.
    pub warnings: Vec<String>,
    /// How the at-risk sessions were accepted.
    pub acceptance: AcceptanceGroups,
    /// The scoped at-risk verdicts this gate lets through, for the ledger the
    /// completed transaction writes; its `recorded_at` is stamped by the
    /// engine when the gate is journaled.
    pub accepted_set: AcceptedSet,
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
    daemon_dir: &VerifiedVersionDir,
    version: &str,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    accept_runtime_loss: bool,
) -> Result<Gated, Error> {
    let report = judge_daemon(
        preflight,
        context,
        daemon_dir,
        version,
        search_path,
        variables,
        ReleasePlan::Omitted,
        false,
    )
    .await?;
    decide(report, accept_runtime_loss, &Acceptance::none())
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
/// of [`gate`] otherwise. At-risk verdicts `acceptance` holds do not block.
#[allow(
    clippy::too_many_arguments,
    reason = "the judgement inputs are passed one by one, as gate does, and the plan is the eighth"
)]
pub async fn gate_upgrade(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &VerifiedVersionDir,
    version: &str,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    plan: ReleasePlan,
    accept_runtime_loss: bool,
    acceptance: &Acceptance,
) -> Result<Gated, Error> {
    gate_upgrade_over(
        preflight,
        context,
        daemon_dir,
        version,
        search_path,
        variables,
        plan,
        accept_runtime_loss,
        acceptance,
        JudgedPath::Installed,
    )
    .await
}

/// Where the `PATH` an upgrade is judged with comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgedPath {
    /// The path `service.toml` records for the installed daemon job.
    Installed,
    /// The previous path of a pending search-path refresh, which the upgrade
    /// rolls back first; `service.toml` may already hold the refresh target.
    RestoredByRefresh,
}

/// [`gate_upgrade`] for a `judged` path of either origin, which words the
/// stale-path warning of an at-risk refusal.
///
/// # Errors
///
/// See [`gate_upgrade`].
#[allow(
    clippy::too_many_arguments,
    reason = "the judgement inputs are passed one by one, as gate does, and the plan is the eighth"
)]
pub async fn gate_upgrade_over(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &VerifiedVersionDir,
    version: &str,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    plan: ReleasePlan,
    accept_runtime_loss: bool,
    acceptance: &Acceptance,
    judged: JudgedPath,
) -> Result<Gated, Error> {
    match gate_installed_release(
        preflight,
        context,
        daemon_dir,
        version,
        search_path,
        variables,
        plan,
        accept_runtime_loss,
        acceptance,
    )
    .await
    {
        Err(refusal @ Error::UpgradeAtRisk { .. }) => {
            let warning = recorded_path_warning(context, search_path, judged);
            Err(refusal.with_warnings(warning.as_slice()))
        }
        decided => decided,
    }
}

/// Runs the gate of a search-path refresh, which judges the installed release
/// with the new `PATH` and carries no runtime plan.
///
/// An at-risk refusal is [`Error::RefreshAtRisk`] and carries no stale-path
/// warning: the refresh is the remedy that warning points to.
///
/// # Errors
///
/// See [`gate_upgrade`].
#[allow(
    clippy::too_many_arguments,
    reason = "the judgement inputs are passed one by one, as gate_upgrade takes them"
)]
pub async fn gate_refresh(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &VerifiedVersionDir,
    version: &str,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    accept_runtime_loss: bool,
    acceptance: &Acceptance,
) -> Result<Gated, Error> {
    gate_installed_release(
        preflight,
        context,
        daemon_dir,
        version,
        search_path,
        variables,
        ReleasePlan::Omitted,
        accept_runtime_loss,
        acceptance,
    )
    .await
    .map_err(|error| match error {
        Error::UpgradeAtRisk {
            sessions,
            stopped_sessions,
            acceptance,
            warnings,
        } => Error::RefreshAtRisk {
            sessions,
            stopped_sessions,
            acceptance,
            warnings,
        },
        Error::UpgradeStoreUnusable { path, code, detail } => {
            Error::RefreshStoreUnusable { path, code, detail }
        }
        Error::UpgradePreflightFailed { binary, detail } => {
            Error::RefreshPreflightFailed { binary, detail }
        }
        other => other,
    })
}

/// The judgement of [`gate_upgrade`] and [`gate_refresh`], before either shapes
/// an at-risk refusal.
#[allow(
    clippy::too_many_arguments,
    reason = "the judgement inputs are passed one by one, as gate_upgrade takes them"
)]
async fn gate_installed_release(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &VerifiedVersionDir,
    version: &str,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    plan: ReleasePlan,
    accept_runtime_loss: bool,
    acceptance: &Acceptance,
) -> Result<Gated, Error> {
    let report = judge_daemon(
        preflight,
        context,
        daemon_dir,
        version,
        search_path,
        variables,
        plan,
        true,
    )
    .await?;
    if let Some(plan) = &report.runtimes {
        runtimes::check_plan(plan)?;
    }
    decide(report, accept_runtime_loss, acceptance)
}

/// Start of the warning an at-risk refusal of a managed installation carries.
/// [`Error::hint`] and `packaging/install-daemon.sh` key on it.
pub(super) const STALE_PATH_WARNING: &str = "the search path this upgrade judged with is:";

/// Marks the warning of an upgrade that judges with the previous path of a
/// pending search-path refresh. [`Error::hint`] keys on it.
pub(super) const RESTORED_PATH_NOTE: &str =
    "the path a pending search-path refresh's rollback restores";

/// Names the recorded search path and the command that refreshes it, when the
/// upgrade refuses sessions as at risk.
///
/// The upgrade judges runtimes with the `PATH` the daemon job carries, which
/// `service.toml` records at install. A Nix profile update leaves that entry
/// pointing at the store path of an older generation: the runtime found there
/// can be older than the one the login shell resolves now, and the sessions
/// look at risk although the runtime is current. The warning is static: it
/// never runs the login shell, which `pohunek service refresh-path --dry-run`
/// does outside any lock. An installation that records no path (the Linux user
/// manager's `PATH`) gets none.
///
/// Over a pending refresh the judged path is the one its rollback restores,
/// not the one `service.toml` holds, and a dry run is refused while the
/// refresh is pending, so the warning sends the operator to finish it.
pub(super) fn recorded_path_warning(
    context: &Context,
    judged: &str,
    origin: JudgedPath,
) -> Option<String> {
    if !context.records_search_path() || judged.is_empty() {
        return None;
    }
    Some(match origin {
        JudgedPath::Installed => format!(
            "{STALE_PATH_WARNING} {judged} (install records it in service.toml); if a runtime \
             moved since it was recorded, for example after a Nix profile update, do not accept \
             the loss: with the `pohunek` of the release you are installing (`./pohunek` in its \
             unpacked archive), compare with `pohunek service refresh-path --dry-run`, record the \
             current path with `pohunek service refresh-path` (it restarts the daemon with that \
             path after the same adoption check), and rerun this upgrade. A pending transaction \
             of another kind blocks refresh-path (`pohunek service status` names it); see the \
             update runbook"
        ),
        JudgedPath::RestoredByRefresh => format!(
            "{STALE_PATH_WARNING} {judged} ({RESTORED_PATH_NOTE}); if a runtime moved since it \
             was recorded, for example after a Nix profile update, do not accept the loss: \
             with the same `pohunek` that started the pending refresh, run `pohunek service \
             refresh-path` to finish it (a dry run is refused while a refresh is pending, and \
             `pohunek service status` names the pending transaction), and rerun this upgrade; \
             see the update runbook"
        ),
    })
}

/// Checks the staged `daemon` is of `version` and returns its report.
///
/// A raised probe budget reaches only a daemon of this CLI's release, and only
/// from the upgrade path (`own_release`): an older daemon, such as a rollback's
/// previous release, rejects the flag and would start a full daemon instead.
/// Without the flag that daemon runs its default budget, which keeps the
/// judgement pessimistic.
#[allow(
    clippy::too_many_arguments,
    reason = "the judgement inputs are passed one by one, as gate does, and the plan is the eighth"
)]
async fn judge_daemon(
    preflight: &dyn AdoptionPreflight,
    context: &Context,
    daemon_dir: &VerifiedVersionDir,
    version: &str,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    plan: ReleasePlan,
    own_release: bool,
) -> Result<PreflightReport, Error> {
    probe_daemon(daemon_dir, version).await?;
    let budget = context.preflight_budget();
    let probe_budget =
        (own_release && version == VERSION && budget != PROBE_BUDGET).then_some(budget);
    preflight
        .judge(
            context,
            daemon_dir,
            search_path,
            variables,
            probe_budget,
            plan,
        )
        .await
}

/// Verifies the daemon binary before using it as compatibility evidence.
///
/// # Errors
///
/// Returns the staged-binary or version-probe failure.
pub(super) async fn probe_daemon(dir: &VerifiedVersionDir, version: &str) -> Result<(), Error> {
    let daemon = dir.daemon();
    match std::fs::metadata(&daemon) {
        Ok(metadata) if metadata.is_file() => {}
        _ => return Err(Error::StagedBinary { path: daemon }),
    }
    layout::probe(&daemon, DAEMON_EXECUTABLE_NAME, version).await?;
    Ok(())
}

/// Applies the operator's choice to `report`.
///
/// An at-risk verdict that `acceptance` holds, with the identical scope, is
/// previously accepted and does not block. `accept_runtime_loss` accepts the
/// whole remainder.
///
/// # Errors
///
/// Returns [`Error::UpgradeStoreUnusable`] when the new daemon would refuse to
/// start with the store, whatever the flag says, and [`Error::UpgradeAtRisk`]
/// when sessions remain at risk and `accept_runtime_loss` is `false`; it lists
/// only the remainder.
pub fn decide(
    report: PreflightReport,
    accept_runtime_loss: bool,
    acceptance: &Acceptance,
) -> Result<Gated, Error> {
    if report.store_refused() {
        return Err(Error::UpgradeStoreUnusable {
            path: report.store.path,
            code: report.store.error_code.unwrap_or_default(),
            detail: report.store.error.unwrap_or_default(),
        });
    }
    let store = report.store.path.clone();
    let mut warnings = acceptance.warnings().to_vec();
    warnings.extend(acceptance.store_mismatch(&store));
    let mut remainder = [Vec::new(), Vec::new()];
    let mut covered = Vec::new();
    for (slot, list, verdicts) in [
        (0, List::Live, &report.sessions),
        (1, List::Stopped, &report.stopped_sessions),
    ] {
        for verdict in verdicts.iter().filter(|verdict| verdict.verdict.at_risk()) {
            if acceptance.covers(&store, list, verdict) {
                covered.push((list, verdict.clone()));
            } else {
                remainder[slot].push((list, verdict.clone()));
            }
        }
    }
    let mut groups = AcceptanceGroups::default();
    if !covered.is_empty() {
        groups.previously_accepted_at = acceptance.applied_version().map(ToOwned::to_owned);
        groups.previously_accepted = covered.iter().map(|(_, verdict)| verdict.clone()).collect();
    }
    let [live, stopped] = remainder;
    if !live.is_empty() || !stopped.is_empty() {
        if !accept_runtime_loss {
            return Err(Error::UpgradeAtRisk {
                sessions: live.into_iter().map(|(_, verdict)| verdict).collect(),
                stopped_sessions: stopped.into_iter().map(|(_, verdict)| verdict).collect(),
                acceptance: Box::new(groups),
                warnings,
            });
        }
        for (list, verdict) in live.iter().chain(&stopped) {
            if Entry::of(*list, verdict).is_some() {
                groups.durable_acceptance.push(verdict.clone());
            } else {
                groups.one_time_acceptance.push(verdict.clone());
            }
        }
    }
    let accepted = !groups.durable_acceptance.is_empty() || !groups.one_time_acceptance.is_empty();
    let mut entries = Vec::new();
    let mut one_time = Vec::new();
    for (list, verdict) in covered.iter().chain(live.iter()).chain(stopped.iter()) {
        match Entry::of(*list, verdict) {
            Some(entry) => entries.push(entry),
            None => one_time.push(OneTime {
                session_id: verdict.session_id.clone(),
                code: verdict.code.clone(),
            }),
        }
    }
    Ok(Gated {
        accepted_set: AcceptedSet {
            store_path: store,
            recorded_at: String::new(),
            entries,
            one_time,
        },
        report,
        accepted,
        warnings,
        acceptance: groups,
    })
}

/// Runs the daemon's preflight with the environment the installed daemon gets.
async fn run_daemon(
    context: &Context,
    daemon: PathBuf,
    search_path: &str,
    variables: &BTreeMap<String, String>,
    probe_budget: Option<Duration>,
    plan: ReleasePlan,
) -> Result<PreflightReport, Error> {
    let failure = |detail: String| Error::UpgradePreflightFailed {
        binary: daemon.clone(),
        detail,
    };
    let mut command = tokio::process::Command::new(&daemon);
    command.arg(PREFLIGHT_ARGUMENT);
    if let Some(flag) = plan.argument() {
        command.arg(flag);
    }
    // Without a raised budget the argument list is the one every release accepts.
    if let Some(budget) = probe_budget {
        command
            .arg(PROBE_BUDGET_FLAG)
            .arg(budget.as_secs().to_string());
    }
    // The manager's variables come first, so the bootstrap roots, which name the
    // paths this CLI resolved, always win over a variable of the same name.
    command
        .env_clear()
        .envs(variables)
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
    let deadline = preflight_timeout(probe_budget.unwrap_or(PROBE_BUDGET));
    let (report, diagnostics, status) =
        tokio::time::timeout(deadline, run)
            .await
            .map_err(|_elapsed| {
                failure(format!("it did not finish within {} s", deadline.as_secs()))
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
