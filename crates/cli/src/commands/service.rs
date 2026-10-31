//! `pohunek service` — install, upgrade, uninstall, check, and inspect the
//! native service, and run a command under its transaction lock.
//!
//! The clap surface and human rendering live here; the transactions live in
//! [`crate::service`]. Every subcommand is local to this machine and ignores
//! the global `--host`.

// Rust guideline compliant 2026-09-29

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Subcommand, ValueHint};
use pohunek_service_config::preflight::{
    PreflightReport, ProfileWarning, SessionVerdict, StoreState, Verdict, MAX_PROBE_BUDGET_SECS,
    MIN_PROBE_BUDGET_SECS, PROBE_BUDGET,
};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, PackageAction, PackageStatus, ProfileAction,
};

use crate::commands::render_json;
use crate::error::CliError;
use crate::service::runtimes::{kept_selection_line, profile_detail, replaces_newer_warning};
use crate::service::{self, report, UninstallOptions};

/// `pohunek service` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Install the daemon as a login service from staged binaries.
    ///
    /// Copies pohunek, pohunekd, and pohunek-sessiond into
    /// `<prefix>/libexec/pohunek/<version>/`, writes `service.toml`, registers
    /// the daemon with systemd (Linux) or launchd (macOS), waits until it
    /// answers, and installs `<prefix>/bin/pohunek`.
    Install {
        /// Directory holding the staged binaries [default: the directory of
        /// this `pohunek`].
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        from: Option<PathBuf>,
        /// Absolute installation prefix [default: `$HOME/.local`].
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        prefix: Option<PathBuf>,
        /// Move a refused version directory of this version aside and publish
        /// the release's copy in its place.
        ///
        /// The refused entry is renamed to `.pohunek_refused-<version>-<random>`
        /// in the versions directory and kept for you to inspect and delete;
        /// nothing in it is run, probed or copied.
        #[arg(long)]
        repair: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Switch the installed service to this version, restarting only the daemon.
    ///
    /// Live sessions keep running on their workers. Version directories no
    /// live worker references are removed afterwards.
    Upgrade {
        /// Directory holding the staged binaries [default: the directory of
        /// this `pohunek`].
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        from: Option<PathBuf>,
        /// Upgrade even if live or stopped sessions would lose runtime or native
        /// recovery.
        ///
        /// The new daemon's adoption preflight lists live sessions it would not
        /// adopt or would adopt without native recovery, and stopped sessions
        /// whose native recovery would be lost. The upgrade refuses either
        /// group without this flag. It never overrides a metadata store the new
        /// daemon cannot start with. Accepting records each verdict the daemon
        /// binds to a scope, so the identical risk is not asked about again at
        /// the next upgrade; it changes no session.
        #[arg(long)]
        accept_runtime_loss: bool,
        /// Replace a refused or missing version directory of this version with
        /// the release's copy, and restart the daemon from it.
        ///
        /// Use it with `--from` the unpacked release archive of the version
        /// the refusal names. A refused directory is renamed to
        /// `.pohunek_refused-<version>-<random>` in the versions directory and
        /// kept for you to inspect and delete; nothing in it is run, probed or
        /// copied. A running daemon of the active version is judged by the
        /// adoption preflight first (`--accept-runtime-loss` applies), then
        /// restarted; worker jobs and `service.toml` are never touched.
        #[arg(long)]
        repair: bool,
        /// Probe budget, in seconds, of the preflight that judges live and
        /// stopped sessions: version probes and native reference checks run
        /// within it. The default is 20; the installer's deadline grows with it.
        #[arg(
            long,
            value_name = "SECONDS",
            value_parser = clap::value_parser!(u64).range(MIN_PROBE_BUDGET_SECS..=MAX_PROBE_BUDGET_SECS)
        )]
        preflight_budget: Option<u64>,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Remove the service, its versions, and its configuration.
    ///
    /// Refuses while sessions are live unless `--stop-sessions` is given. A
    /// stopped daemon is started first so its sessions can be listed.
    Uninstall {
        /// Stop every live session first instead of refusing.
        #[arg(long)]
        stop_sessions: bool,
        /// Also remove the session store, event logs, worker journals, and
        /// host identity. Needs an installation or an interrupted install to
        /// remove; without one, nothing is purged.
        #[arg(long)]
        purge: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Re-record the executable search path from your login shell and restart the daemon with it.
    ///
    /// Install records the `PATH` your login shell reports, and upgrades reuse
    /// it. A prefix installed later, or a profile link that moved to another
    /// store path (Nix generations), leaves the record behind, and the running
    /// daemon keeps the `PATH` its job was registered with. This command runs
    /// the same discovery again, prints the directories added and removed,
    /// rewrites the recorded path atomically and replaces the daemon job, in
    /// one transaction: it records the path and restarts the daemon, or it
    /// changes nothing. Only the daemon restarts; worker jobs keep running. The
    /// new daemon's adoption verdict is taken first, as for an upgrade: live
    /// sessions it would not adopt refuse the run unless `--accept-runtime-loss`
    /// is given. A login shell that fails or prints junk never replaces the
    /// recorded path. `--dry-run` takes no lock and changes nothing.
    RefreshPath {
        /// Print the change and the adoption verdict without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Proceed even if live or stopped sessions would lose runtime or
        /// native recovery with the new path.
        ///
        /// The new daemon's adoption preflight lists them; the run refuses
        /// without this flag. It never overrides a metadata store the new
        /// daemon cannot start with, and it records nothing in the
        /// accepted-risk ledger.
        #[arg(long)]
        accept_runtime_loss: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Show the daemon job, namespace, versions, and workers per generation.
    Status {
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Check that this version's install or upgrade would pass its preflight.
    ///
    /// Runs the checks `install` (while an install is pending or nothing is
    /// installed) or `upgrade` makes before their first effect: `HOME` and
    /// the XDG roots, the prefix, every directory they write, a pending
    /// transaction, the recorded installation, and the prefix owner. An
    /// upgrade also asks the new daemon's adoption preflight about every live
    /// session. Fails with the error that command would fail with; changes
    /// nothing.
    Check {
        /// Absolute installation prefix of an install [default:
        /// `$HOME/.local`]; an upgrade keeps the installed prefix.
        #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
        prefix: Option<PathBuf>,
        /// Pass the check although live or stopped sessions would lose runtime or
        /// native recovery, as `service upgrade --accept-runtime-loss` would.
        /// The check records nothing.
        #[arg(long)]
        accept_runtime_loss: bool,
        /// Probe budget, in seconds, of the preflight that judges live and
        /// stopped sessions: version probes and native reference checks run
        /// within it. The default is 20; the installer's deadline grows with it.
        #[arg(
            long,
            value_name = "SECONDS",
            value_parser = clap::value_parser!(u64).range(MIN_PROBE_BUDGET_SECS..=MAX_PROBE_BUDGET_SECS)
        )]
        preflight_budget: Option<u64>,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Run a command while holding the service transaction lock.
    ///
    /// No other `pohunek service install`, `upgrade`, or `uninstall` can run
    /// until the command exits. Those commands, `check`, and a nested `lock`
    /// started by the command reuse the lock through the holder token in
    /// `POHUNEK_SERVICE_LOCK_TOKEN`, which is valid only while this process
    /// runs. Exits with the command's status (128 plus the signal number when
    /// a signal ended it).
    Lock {
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
}

impl Action {
    /// Whether the subcommand requested `--json` output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::Install { json, .. }
            | Self::Upgrade { json, .. }
            | Self::Uninstall { json, .. }
            | Self::Check { json, .. }
            | Self::RefreshPath { json, .. }
            | Self::Status { json } => *json,
            Self::Lock { .. } => false,
        }
    }
}

/// Runs one `pohunek service` subcommand and returns its exit code.
///
/// Every subcommand but `lock` succeeds with status 0; `lock` exits with
/// its command's status.
///
/// # Errors
///
/// Returns [`CliError::Service`] when the operation fails.
pub(crate) async fn run(action: Action) -> Result<ExitCode, CliError> {
    match action {
        Action::Lock { command } => {
            let (program, arguments) = command
                .split_first()
                .expect("clap requires at least one command word");
            let status = service::lock(program, arguments).await?;
            return Ok(ExitCode::from(service::exit_code(status)));
        }
        Action::Install {
            from,
            prefix,
            repair,
            json,
        } => {
            let report = service::install(from, prefix, repair).await?;
            emit(json, &report, render_install)
        }
        Action::Upgrade {
            from,
            accept_runtime_loss,
            repair,
            preflight_budget,
            json,
        } => {
            let budget = preflight_budget.map_or(PROBE_BUDGET, Duration::from_secs);
            let report = service::upgrade(from, accept_runtime_loss, budget, repair).await?;
            emit(json, &report, render_upgrade)
        }
        Action::RefreshPath {
            dry_run,
            accept_runtime_loss,
            json,
        } => {
            let report = service::refresh_search_path(dry_run, accept_runtime_loss).await?;
            emit(json, &report, render_refresh_path)
        }
        Action::Check {
            prefix,
            accept_runtime_loss,
            preflight_budget,
            json,
        } => {
            let budget = preflight_budget.map_or(PROBE_BUDGET, Duration::from_secs);
            let report = service::check(prefix, accept_runtime_loss, budget).await?;
            emit(json, &report, render_check)
        }
        Action::Uninstall {
            stop_sessions,
            purge,
            json,
        } => {
            let report = service::uninstall(UninstallOptions {
                stop_sessions,
                purge,
            })
            .await?;
            emit(json, &report, render_uninstall)
        }
        Action::Status { json } => {
            let report = service::status().await?;
            emit(json, &report, render_status)
        }
    }?;
    Ok(ExitCode::SUCCESS)
}

fn emit<T: serde::Serialize>(
    json: bool,
    report: &T,
    human: fn(&T) -> String,
) -> Result<(), CliError> {
    if json {
        print!("{}", render_json(report)?);
    } else {
        print!("{}", human(report));
    }
    Ok(())
}

fn render_install(report: &report::InstallReport) -> String {
    let mut text = String::new();
    if let Some(pending) = &report.rolled_back {
        let _ = writeln!(text, "{}", render_rolled_back(pending));
    }
    let _ = writeln!(
        text,
        "installed pohunek {} (namespace {}){}",
        report.version,
        report.namespace,
        if report.resumed { ", resumed" } else { "" }
    );
    let _ = writeln!(text, "prefix     {}", report.prefix.display());
    let _ = writeln!(text, "config     {}", report.config_path.display());
    let _ = writeln!(text, "daemon     {}", report.daemon_executable.display());
    render_search_path(&mut text, &report.search_path);
    if let Some(repair) = &report.repair {
        render_repair(&mut text, repair);
    }
    if let Some(runtimes) = &report.runtimes {
        render_install_runtimes(&mut text, runtimes);
    } else if report.resumed {
        // A resume skips the runtime step only when the interrupted run had
        // completed it, so its outcome is not in this report.
        let _ = writeln!(text, "runtimes   completed by the interrupted run");
    }
    text
}

/// Appends what `--repair` did to the version directory and the daemon job.
fn render_repair(text: &mut String, repair: &report::RepairReport) {
    let directory = if repair.restored {
        "restored from the release"
    } else {
        "left as it was"
    };
    let _ = writeln!(
        text,
        "repair     version {} directory {directory}",
        repair.version
    );
    if let Some(path) = &repair.set_aside {
        let _ = writeln!(
            text,
            "           the refused entry was moved to {}; inspect it and delete it yourself",
            path.display()
        );
    }
    let daemon = match repair.daemon {
        "installed" => "the daemon job was missing and was registered again",
        "started" => "the daemon job was stopped and was started",
        "restarted" => "the daemon was restarted; worker jobs keep running",
        _ => "the daemon job was not touched",
    };
    let _ = writeln!(text, "           {daemon}");
}

/// Appends the official runtime step the install ran: the catalog trust
/// state, and each package and profile pin with its status.
fn render_install_runtimes(text: &mut String, runtimes: &EnsureReport) {
    let _ = writeln!(text, "catalog    {}", wire_name(&runtimes.catalog));
    for package in &runtimes.packages {
        let _ = writeln!(
            text,
            "package    {} {} {} ({})",
            package.runtime,
            package.package,
            package.version,
            wire_name(&package.status)
        );
    }
    for package in &runtimes.packages {
        append_package_warning(text, package);
    }
    for profile in &runtimes.profiles {
        let _ = writeln!(
            text,
            "profile    {} (base {}, {})",
            profile.name,
            profile.base,
            wire_name(&profile.status)
        );
        append_profile_detail(text, profile);
    }
}

/// Appends the warning of a package that replaces a newer selected version,
/// when there is one.
fn append_package_warning(text: &mut String, package: &PackageAction) {
    if let Some(warning) = replaces_newer_warning(package) {
        let _ = writeln!(text, "{warning}");
    }
    if let Some(line) = kept_selection_line(package) {
        let _ = writeln!(text, "           {line}");
    }
}

/// Appends what the plan found and leaves to the operator, one gap per line
/// with its next step.
fn append_gaps(text: &mut String, plan: &EnsureReport) {
    for gap in &plan.gaps {
        let _ = writeln!(
            text,
            "gap        {} [{}] {}",
            gap.runtime, gap.code, gap.detail
        );
    }
}

/// Appends the detail of a profile that repins or keeps its pin, when there is
/// one, on the line below the profile.
fn append_profile_detail(text: &mut String, profile: &ProfileAction) {
    if let Some(detail) = profile_detail(profile) {
        let _ = writeln!(text, "           {detail}");
    }
}

/// The wire name of a value, as the `--json` output spells it.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn render_refresh_path(report: &report::RefreshPathReport) -> String {
    let mut text = String::new();
    if let Some(pending) = &report.rolled_back {
        let _ = writeln!(text, "{}", render_rolled_back(pending));
        // The directories the rollback restored are not a difference to
        // record, so the diff and its "unchanged" line are left out.
        let kept = report.workers_kept.map_or_else(
            || "worker jobs are not touched".to_owned(),
            |count| format!("{count} workers kept running"),
        );
        let daemon = if report.daemon_restart == "restarted" {
            format!("daemon restarted with the previous search path; {kept}")
        } else {
            "the daemon was not touched".to_owned()
        };
        let _ = writeln!(text, "{daemon}");
    } else if report.changed {
        let verb = if report.applied {
            "recorded"
        } else {
            "would record"
        };
        let _ = writeln!(text, "{verb} {} directories", report.discovered.len());
        for entry in &report.added {
            let _ = writeln!(text, "+ {}", entry.display());
        }
        for entry in &report.removed {
            let _ = writeln!(text, "- {}", entry.display());
        }
        if report.added.is_empty() && report.removed.is_empty() {
            let _ = writeln!(text, "the same directories, in a new order");
        }
        let kept = report.workers_kept.map_or_else(
            || "worker jobs are not touched".to_owned(),
            |count| format!("{count} workers kept running"),
        );
        match report.daemon_restart {
            "restarted" => {
                let started = if report.daemon_started {
                    " (it was not running and was started)"
                } else {
                    ""
                };
                let resumed = if report.resumed { ", resumed" } else { "" };
                let _ = writeln!(
                    text,
                    "daemon restarted with the new search path{started}; {kept}{resumed}"
                );
            }
            "would_restart" => {
                let _ = writeln!(
                    text,
                    "dry run: service.toml and the daemon are unchanged; a real run would restart the daemon; {kept}"
                );
            }
            _ => {
                let _ = writeln!(text, "service.toml and the daemon are unchanged");
            }
        }
    } else {
        let _ = writeln!(
            text,
            "the recorded search path is current ({} directories); nothing to change",
            report.recorded.len()
        );
    }
    if let Some(preflight) = &report.preflight {
        text.push('\n');
        render_preflight(&mut text, preflight, report.accepted_runtime_loss);
        text.push('\n');
    }
    if let Some(shell) = &report.shell_used {
        let note = if report.shell_defaulted {
            " (default: $SHELL is unset)"
        } else {
            ""
        };
        let _ = writeln!(text, "shell      {}{note}", shell.display());
    }
    for dropped in &report.dropped {
        let _ = writeln!(
            text,
            "warning: ignored PATH directory {} ({})",
            dropped.path, dropped.reason
        );
    }
    for warning in &report.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
    text
}

/// Appends the `PATH` source line and a warning for every fallback or refusal.
fn render_search_path(text: &mut String, report: &report::SearchPathReport) {
    if report.source == "unmanaged" {
        return;
    }
    let _ = writeln!(
        text,
        "path       {} ({} directories)",
        report.source,
        report.entries.len()
    );
    if let Some(shell) = &report.shell_used {
        let note = if report.shell_defaulted {
            " (default: $SHELL is unset)"
        } else {
            ""
        };
        let _ = writeln!(text, "shell      {}{note}", shell.display());
    }
    if let Some(failure) = &report.login_shell_failure {
        let _ = writeln!(
            text,
            "warning: login shell PATH discovery failed ({failure}); the fallback directory list is used"
        );
    }
    for dropped in &report.dropped {
        let _ = writeln!(
            text,
            "warning: ignored PATH directory {} ({})",
            dropped.path, dropped.reason
        );
    }
}

/// Appends what the adoption preflight found: the store dry run, and every
/// live or stopped session with its verdict, reason code and evidence.
fn render_preflight(text: &mut String, preflight: &PreflightReport, accepted_runtime_loss: bool) {
    let store = &preflight.store;
    match (store.state, store.schema_from) {
        (StoreState::WouldMigrate, Some(from)) => {
            let _ = writeln!(
                text,
                "store      schema {from} -> {} ({} records) migrated at the new daemon's startup",
                store.schema_to, store.records
            );
        }
        (StoreState::UpToDate, _) => {
            let _ = writeln!(text, "store      schema {} is current", store.schema_to);
        }
        _ => {}
    }
    if preflight.sessions.is_empty() {
        let _ = writeln!(text, "sessions   no live sessions");
    }
    render_sessions(
        text,
        ("sessions", "live"),
        &preflight.sessions,
        accepted_runtime_loss,
    );
    render_sessions(
        text,
        ("stopped", "with native resume or fork"),
        &preflight.stopped_sessions,
        accepted_runtime_loss,
    );
    render_profile_warnings(text, &preflight.profile_warnings);
}

/// Appends how the at-risk verdicts of the gate were, or would be, accepted.
///
/// Accepting records the verdicts for the next upgrade to recognize; it never
/// changes a session.
fn render_acceptance(text: &mut String, acceptance: &report::AcceptanceGroups) {
    let mut group = |label: &str, note: &str, sessions: &[SessionVerdict]| {
        if sessions.is_empty() {
            return;
        }
        let _ = writeln!(text, "{label:<10} {} session(s) {note}", sessions.len());
        for session in sessions {
            let _ = writeln!(text, "  {}: [{}]", session.session_id, session.code);
        }
    };
    let previously = acceptance.previously_accepted_at.as_deref().map_or_else(
        || "previously accepted (not blocking)".to_owned(),
        |version| format!("previously accepted at upgrade to {version} (not blocking)"),
    );
    group("accepted", &previously, &acceptance.previously_accepted);
    group(
        "recorded",
        "accepted with --accept-runtime-loss; the identical risk is not asked about again, and the sessions are not changed",
        &acceptance.durable_acceptance,
    );
    group(
        "one-time",
        "accepted with --accept-runtime-loss; nothing binds them, so the next upgrade asks again",
        &acceptance.one_time_acceptance,
    );
}

/// Appends the count line of the host profiles that load with a reduced
/// guarantee and one line per profile. Warnings never block the upgrade.
fn render_profile_warnings(text: &mut String, warnings: &[ProfileWarning]) {
    if warnings.is_empty() {
        return;
    }
    let _ = writeln!(
        text,
        "profiles   {} with a warning, not blocking the upgrade",
        warnings.len()
    );
    for warning in warnings {
        let _ = writeln!(
            text,
            "  {}: [{}] {}",
            warning.profile, warning.code, warning.detail
        );
    }
}

/// Appends the count line of `sessions` and one line per session.
///
/// `heading` is the label and the kind of the sessions in the count line.
/// Nothing is written for an empty list.
fn render_sessions(
    text: &mut String,
    (heading, kind): (&str, &str),
    sessions: &[SessionVerdict],
    accepted_runtime_loss: bool,
) {
    if sessions.is_empty() {
        return;
    }
    let at_risk = sessions
        .iter()
        .filter(|session| session.verdict.at_risk())
        .count();
    let not_resumable = sessions
        .iter()
        .filter(|session| session.verdict == Verdict::AlreadyNotResumable)
        .count();
    let _ = writeln!(
        text,
        "{heading:<10} {} {kind}, {} adoptable{}{}",
        sessions.len(),
        sessions.len() - at_risk - not_resumable,
        if at_risk == 0 {
            String::new()
        } else if accepted_runtime_loss {
            format!(", {at_risk} at risk (accepted with --accept-runtime-loss)")
        } else {
            format!(", {at_risk} at risk")
        },
        if not_resumable == 0 {
            String::new()
        } else {
            format!(", {not_resumable} already not resumable before the upgrade")
        }
    );
    for session in sessions {
        let name = session
            .name
            .as_deref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "  {}{name}: {} [{}] {}",
            session.session_id, session.verdict, session.code, session.detail
        );
    }
}

/// Appends the release step's plan: the packages it installs with the reasons
/// the host uses them, the profiles it pins, and the integrations it reinstalls.
/// A plan with refusals never reaches this, because its error lists them.
fn render_plan(text: &mut String, plan: &EnsureReport) {
    render_catalog(text, plan);
    for package in &plan.packages {
        let _ = writeln!(
            text,
            "plan       runtime {}  {} {}  {} ({})",
            package.runtime,
            package.package,
            package.version,
            format!("{:?}", package.status).to_lowercase(),
            package
                .reasons
                .iter()
                .map(|reason| format!("{reason:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(", ")
        );
        append_package_warning(text, package);
    }
    for profile in &plan.profiles {
        let _ = writeln!(
            text,
            "plan       profile {}  pin to {}  {}",
            profile.name,
            profile.base,
            format!("{:?}", profile.status).to_lowercase()
        );
        append_profile_detail(text, profile);
    }
    for integration in &plan.integrations {
        let profile = integration
            .profile
            .as_deref()
            .map(|profile| format!(" (profile {profile})"))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "plan       integration {}{profile}  reinstall after the upgrade",
            integration.runtime
        );
    }
    append_gaps(text, plan);
}

fn render_upgrade(report: &report::UpgradeReport) -> String {
    let mut text = String::new();
    for warning in &report.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
    if let Some(pending) = &report.rolled_back {
        let _ = writeln!(text, "{}", render_rolled_back(pending));
    }
    if let Some(schema) = report.config_schema_migration {
        let _ = writeln!(
            text,
            "migration  service.toml is upgraded from schema {schema}: the daemon stops before the write, and a rollback restores the exact previous file"
        );
    }
    if report.unchanged {
        let _ = writeln!(text, "pohunek {} is already active", report.to_version);
    } else {
        let _ = writeln!(
            text,
            "upgraded pohunek {} -> {}{}",
            report.from_version,
            report.to_version,
            if report.resumed { ", resumed" } else { "" }
        );
    }
    if let Some(repair) = &report.repair {
        render_repair(&mut text, repair);
    }
    for path in &report.refused_entries {
        let _ = writeln!(
            text,
            "kept {} (set aside by a repair; delete it when you no longer need it)",
            report::escaped_purge_path(path)
        );
    }
    for version in &report.removed_versions {
        let _ = writeln!(text, "removed version {version}");
    }
    for kept in &report.kept_versions {
        let _ = writeln!(text, "kept version {}: {}", kept.version, kept.reason);
    }
    if let Some(error) = &report.gc_error {
        let _ = writeln!(text, "warning: old versions were not cleaned up: {error}");
    }
    if let Some(runtimes) = &report.runtimes {
        render_runtimes(&mut text, runtimes);
    }
    for integration in &report.integrations {
        let _ = writeln!(
            text,
            "integration {}  {}{}",
            integration.integration,
            integration.status,
            integration
                .detail
                .as_ref()
                .map_or_else(String::new, |detail| format!(": {detail}"))
        );
    }
    if let Some(preflight) = &report.preflight {
        if report
            .rolled_back
            .as_ref()
            .and_then(|pending| pending.preflight.as_ref())
            == Some(preflight)
        {
            return text;
        }
        // The applied packages above already list the plan of an upgrade that ran it.
        if report.runtimes.is_none() {
            if let Some(plan) = &preflight.runtimes {
                render_plan(&mut text, plan);
            }
        }
        render_preflight(&mut text, preflight, report.accepted_runtime_loss);
        render_acceptance(&mut text, &report.acceptance);
    }
    text
}

/// Appends the catalog trust state of the runtime step and why a kept package
/// is kept.
fn render_catalog(text: &mut String, runtimes: &EnsureReport) {
    match runtimes.catalog {
        CatalogStatus::Unavailable => {
            let kept: Vec<&str> = runtimes
                .packages
                .iter()
                .filter(|package| package.status == PackageStatus::Kept)
                .map(|package| package.runtime.as_str())
                .collect();
            if kept.is_empty() {
                let _ = writeln!(
                    text,
                    "catalog  unavailable: the release has no usable runtime catalog (absent, not verifiable, or older than the recorded one)"
                );
            } else {
                let _ = writeln!(
                    text,
                    "catalog  unavailable: the release has no usable runtime catalog (absent, not verifiable, or older than the recorded one); the installed package keeps serving {}",
                    kept.join(", ")
                );
            }
        }
        other => {
            let _ = writeln!(text, "catalog  {}", format!("{other:?}").to_lowercase());
        }
    }
}

/// Lists the official runtime packages, the catalog trust state and profile
/// pins of the runtime step.
fn render_runtimes(text: &mut String, runtimes: &EnsureReport) {
    render_catalog(text, runtimes);
    for package in &runtimes.packages {
        let _ = writeln!(
            text,
            "runtime {}  {} {}  {}",
            package.runtime,
            package.package,
            package.version,
            format!("{:?}", package.status).to_lowercase()
        );
        append_package_warning(text, package);
    }
    for profile in &runtimes.profiles {
        let _ = writeln!(
            text,
            "profile {}  pin to {}  {}",
            profile.name,
            profile.base,
            format!("{:?}", profile.status).to_lowercase()
        );
        append_profile_detail(text, profile);
    }
    append_gaps(text, runtimes);
}

fn render_check(report: &report::CheckReport) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} of pohunek {} would pass its preflight (namespace {}){}",
        report.operation,
        report.version,
        report.namespace,
        if report.locked {
            ", checked under the transaction lock"
        } else {
            ""
        }
    );
    if let Some(schema) = report.config_schema_migration {
        let _ = writeln!(
            text,
            "migration  service.toml is upgraded from schema {schema}: the daemon stops before the write, and a rollback restores the exact previous file"
        );
    }
    for warning in &report.warnings {
        let _ = writeln!(text, "warning    {warning}");
    }
    let _ = writeln!(text, "prefix     {}", report.prefix.display());
    let _ = writeln!(text, "config     {}", report.config_path.display());
    if let (Some(pending), Some(action)) = (&report.pending_transaction, report.pending_action) {
        let _ = writeln!(
            text,
            "pending    {} {} stopped after step {} would be {}",
            pending.operation,
            pending.version,
            pending.step,
            if action == "resume" {
                "resumed"
            } else {
                "rolled back first"
            }
        );
        render_pending_warnings(&mut text, pending);
    }
    if let Some(preflight) = &report.preflight {
        if let Some(plan) = &preflight.runtimes {
            render_plan(&mut text, plan);
        }
        render_preflight(&mut text, preflight, report.accepted_runtime_loss);
        render_acceptance(&mut text, &report.acceptance);
    }
    text
}

fn render_uninstall(report: &report::UninstallReport) -> String {
    let mut text = String::new();
    if let Some(pending) = &report.rolled_back {
        let _ = writeln!(text, "{}", render_rolled_back(pending));
    }
    if report.started_daemon {
        let _ = writeln!(text, "started the daemon to list its sessions");
    }
    for session in &report.stopped_sessions {
        let _ = writeln!(text, "stopped session {session}");
    }
    for worker in &report.retired_workers {
        let _ = writeln!(text, "retired worker {worker}");
    }
    for path in &report.removed {
        let _ = writeln!(text, "removed {}", path.display());
    }
    for path in &report.kept_purge_entries {
        let _ = writeln!(
            text,
            "kept {} (outside purge scope)",
            report::escaped_purge_path(path)
        );
    }
    for kept in &report.kept_versions {
        let _ = writeln!(text, "kept version {}: {}", kept.version, kept.reason);
    }
    for path in &report.refused_entries {
        let _ = writeln!(
            text,
            "kept {} (set aside by a repair; delete it when you no longer need it)",
            report::escaped_purge_path(path)
        );
    }
    let _ = writeln!(
        text,
        "uninstalled pohunek{}",
        if report.purged {
            " and purged durable metadata"
        } else {
            "; the session store, journals, and host identity were kept"
        }
    );
    text
}

fn render_status(report: &report::StatusReport) -> String {
    let mut text = String::new();
    render_transaction(&mut text, report);
    if !report.installed {
        let _ = writeln!(
            text,
            "not installed ({} is missing)",
            report.config_path.display()
        );
        return text;
    }
    let field = |value: Option<&str>| value.unwrap_or("-").to_owned();
    let _ = writeln!(text, "namespace  {}", field(report.namespace.as_deref()));
    let _ = writeln!(
        text,
        "prefix     {}",
        report
            .prefix
            .as_ref()
            .map_or_else(|| "-".to_owned(), |prefix| prefix.display().to_string())
    );
    let _ = writeln!(
        text,
        "active     {}",
        field(report.active_version.as_deref())
    );
    match (&report.daemon, &report.daemon_error) {
        (Some(daemon), _) => {
            let _ = writeln!(
                text,
                "daemon     {} {}{}",
                daemon.service_id,
                daemon.state,
                daemon
                    .pid
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default()
            );
        }
        (None, Some(error)) => {
            let _ = writeln!(text, "daemon     unavailable: {error}");
        }
        (None, None) => {
            let _ = writeln!(text, "daemon     not registered");
        }
    }
    for version in &report.versions {
        let _ = writeln!(
            text,
            "version    {}{}{}",
            version.version,
            if version.active { " (active)" } else { "" },
            if version.journals.is_empty() {
                String::new()
            } else {
                format!(
                    ", live journals: {}",
                    version
                        .journals
                        .iter()
                        .map(|journal| format!("{}/{}", journal.session_id, journal.worker_id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
    }
    for worker in &report.workers {
        let _ = writeln!(
            text,
            "worker     {} generation {} {}{}{}",
            worker.session_id,
            worker.generation,
            worker.job.state,
            worker
                .job
                .pid
                .map(|pid| format!(" (pid {pid})"))
                .unwrap_or_default(),
            worker
                .version
                .as_deref()
                .map(|version| format!(" version {version}"))
                .unwrap_or_default()
        );
    }
    if let Some(error) = &report.workers_error {
        let _ = writeln!(text, "workers    unavailable: {error}");
    }
    for path in &report.unreadable_journals {
        let _ = writeln!(text, "warning: unreadable journal {}", path.display());
    }
    for warning in &report.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
    text
}

/// Appends one `warning:` line for each journal condition of `pending`.
fn render_pending_warnings(text: &mut String, pending: &report::PendingReport) {
    for warning in &pending.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
}

fn render_transaction(text: &mut String, report: &report::StatusReport) {
    match (&report.pending_transaction, report.transaction_in_progress) {
        (Some(pending), true) => {
            let _ = writeln!(
                text,
                "running    {} {} is at step {} in another `pohunek service` command",
                pending.operation, pending.version, pending.step
            );
            render_pending_warnings(text, pending);
        }
        (None, true) => {
            let _ = writeln!(
                text,
                "running    another `pohunek service` command holds the lock"
            );
        }
        (Some(pending), false) => {
            let _ = writeln!(
                text,
                "pending    {} {} stopped after step {} ({})",
                pending.operation,
                pending.version,
                pending.step,
                pending_hint(pending.operation)
            );
            render_pending_warnings(text, pending);
        }
        (None, false) => {}
    }
}

/// What the operator does with a stopped transaction of `operation`.
///
/// A refresh is resolved by its own command; uninstall would remove the whole
/// installation, so it is no rollback for it.
fn pending_hint(operation: &str) -> &'static str {
    if operation == "refresh_path" {
        "rerun `pohunek service refresh-path` with the same `pohunek` (the release's `./pohunek` if you used it)"
    } else {
        "rerun it to resume, or uninstall to roll back"
    }
}

fn render_rolled_back(pending: &report::PendingReport) -> String {
    let mut text = format!(
        "rolled back an interrupted {} of {} (stopped after step {})",
        pending.operation, pending.version, pending.step
    );
    render_pending_warnings(&mut text, pending);
    if let Some(preflight) = &pending.preflight {
        text.push('\n');
        render_preflight(&mut text, preflight, pending.accepted_runtime_loss);
    }
    text.trim_end().to_owned()
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_kept_package_is_described_without_claiming_why_it_is_kept() {
        for (selected, release) in [("2.0.0", "1.0.3"), ("2.0", "1.0.3")] {
            let package = pohunek_service_config::runtimes::PackageAction {
                runtime: "pi".to_owned(),
                package: "pohunek.runtime.pi".to_owned(),
                version: selected.to_owned(),
                digest: "de".repeat(32),
                status: PackageStatus::Kept,
                reasons: vec![pohunek_service_config::runtimes::UseReason::Installed],
                replaces_newer: None,
                release_version: Some(release.to_owned()),
            };
            let mut text = String::new();
            append_package_warning(&mut text, &package);
            assert!(
                text.contains(&format!(
                    "keeps the selected pohunek.runtime.pi {selected}; the release offers {release}"
                )),
                "{text}"
            );
            assert!(!text.contains("newer"), "{text}");
        }
    }
    use std::path::Path;

    use clap::Parser as _;

    use super::*;

    #[derive(Debug, clap::Parser)]
    struct Harness {
        #[command(subcommand)]
        action: Action,
    }

    fn parse(args: &[&str]) -> Action {
        Harness::try_parse_from(std::iter::once("service").chain(args.iter().copied()))
            .expect("parse")
            .action
    }

    fn install_report(search_path: report::SearchPathReport) -> report::InstallReport {
        report::InstallReport {
            version: "1.2.3".to_owned(),
            prefix: "/p".into(),
            namespace: "ns".to_owned(),
            config_path: "/c/service.toml".into(),
            daemon_executable: "/p/pohunekd".into(),
            resumed: false,
            rolled_back: None,
            search_path,
            runtimes: None,
            repair: None,
        }
    }

    fn search_path_report(source: &'static str) -> report::SearchPathReport {
        report::SearchPathReport {
            source,
            entries: vec!["/opt/homebrew/bin".into(), "/usr/bin".into()],
            shell_used: Some("/bin/zsh".into()),
            shell_defaulted: true,
            login_shell_failure: None,
            dropped: Vec::new(),
        }
    }

    #[test]
    fn an_install_that_ran_the_runtime_step_prints_its_packages_and_profiles() {
        let mut install = install_report(search_path_report("login_shell"));
        install.runtimes = Some(
            serde_json::from_value(serde_json::json!({
                "report_version": 2,
                "daemon_version": "1.2.3",
                "dry_run": false,
                "packages": [{
                    "runtime": "claude",
                    "package": "claude-code",
                    "version": "1.0.0",
                    "digest": format!("sha256:{}", "a".repeat(64)),
                    "status": "installed",
                    "reasons": ["profile"]
                }],
                "catalog": "recorded",
                "profiles": [{
                    "name": "consumer",
                    "base": "claude",
                    "digest": format!("sha256:{}", "a".repeat(64)),
                    "status": "bound"
                }],
                "integrations": [],
                "refusals": []
            }))
            .expect("a runtime report"),
        );
        let text = render_install(&install);
        assert!(text.contains("catalog    recorded"), "{text}");
        assert!(
            text.contains("package    claude claude-code 1.0.0 (installed)"),
            "{text}"
        );
        assert!(
            text.contains("profile    consumer (base claude, bound)"),
            "{text}"
        );
    }

    #[test]
    fn a_resumed_install_says_its_runtime_step_completed_earlier() {
        let mut install = install_report(search_path_report("login_shell"));
        install.resumed = true;
        let text = render_install(&install);
        assert!(
            text.contains("runtimes   completed by the interrupted run"),
            "{text}"
        );
    }

    #[test]
    fn a_clean_login_shell_path_prints_no_warning() {
        let text = render_install(&install_report(search_path_report("login_shell")));
        assert!(
            text.contains("path       login_shell (2 directories)"),
            "{text}"
        );
        assert!(
            text.contains("shell      /bin/zsh (default: $SHELL is unset)"),
            "{text}"
        );
        assert!(!text.contains("warning"), "{text}");
    }

    #[test]
    fn a_fallback_and_refused_directories_print_warnings_matching_the_json() {
        let mut path = search_path_report("fallback");
        path.login_shell_failure = Some("login shell probe exceeded 10s and was killed".to_owned());
        path.dropped = vec![report::DroppedPath {
            path: "/usr/local/bin".to_owned(),
            reason: "writable by group or others",
        }];
        let install = install_report(path);
        let text = render_install(&install);
        assert!(
            text.contains(
                "warning: login shell PATH discovery failed (login shell probe exceeded 10s"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "warning: ignored PATH directory /usr/local/bin (writable by group or others)"
            ),
            "{text}"
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&install).expect("json")).expect("parse");
        let search_path = &json["ok"]["search_path"];
        assert_eq!(search_path["source"], "fallback");
        assert_eq!(search_path["shell_used"], "/bin/zsh");
        assert_eq!(search_path["shell_defaulted"], true);
        assert_eq!(
            search_path["login_shell_failure"],
            "login shell probe exceeded 10s and was killed"
        );
        assert_eq!(search_path["dropped"][0]["path"], "/usr/local/bin");
        assert_eq!(
            search_path["dropped"][0]["reason"],
            "writable by group or others"
        );
    }

    #[test]
    fn an_unmanaged_path_prints_nothing() {
        let mut path = search_path_report("unmanaged");
        path.shell_used = None;
        let text = render_install(&install_report(path));
        assert!(!text.contains("path "), "{text}");
    }

    #[test]
    fn every_subcommand_and_flag_parses() {
        assert!(matches!(
            parse(&["install", "--from", "/a", "--prefix", "/p", "--json"]),
            Action::Install { from: Some(from), prefix: Some(prefix), repair: false, json: true }
                if from == Path::new("/a") && prefix == Path::new("/p")
        ));
        assert!(matches!(
            parse(&["upgrade", "--from", "/a"]),
            Action::Upgrade {
                from: Some(_),
                accept_runtime_loss: false,
                repair: false,
                preflight_budget: None,
                json: false
            }
        ));
        assert!(matches!(
            parse(&["upgrade", "--accept-runtime-loss", "--json"]),
            Action::Upgrade {
                from: None,
                accept_runtime_loss: true,
                repair: false,
                preflight_budget: None,
                json: true
            }
        ));
        assert!(matches!(
            parse(&["uninstall", "--stop-sessions", "--purge", "--json"]),
            Action::Uninstall {
                stop_sessions: true,
                purge: true,
                json: true
            }
        ));
        assert!(matches!(
            parse(&["status", "--json"]),
            Action::Status { json: true }
        ));
        Harness::try_parse_from(["service", "upgrade", "--prefix", "/p"])
            .expect_err("upgrade takes no --prefix");
        assert!(matches!(
            parse(&["check", "--prefix", "/p", "--json"]),
            Action::Check { prefix: Some(prefix), json: true, accept_runtime_loss: false, .. }
                if prefix == Path::new("/p")
        ));
        assert!(matches!(
            parse(&["check"]),
            Action::Check {
                prefix: None,
                accept_runtime_loss: false,
                preflight_budget: None,
                json: false
            }
        ));
        assert!(matches!(
            parse(&["check", "--accept-runtime-loss"]),
            Action::Check {
                accept_runtime_loss: true,
                preflight_budget: None,
                ..
            }
        ));
        assert!(matches!(
            parse(&["upgrade", "--preflight-budget", "90"]),
            Action::Upgrade {
                preflight_budget: Some(90),
                ..
            }
        ));
        assert!(matches!(
            parse(&["check", "--preflight-budget", "300"]),
            Action::Check {
                preflight_budget: Some(300),
                ..
            }
        ));
        for rejected in ["0", "301", "soon"] {
            Harness::try_parse_from(["service", "check", "--preflight-budget", rejected])
                .expect_err("the budget is a whole number of seconds from 1 to 300");
        }
        Harness::try_parse_from(["service", "install", "--accept-runtime-loss"])
            .expect_err("install has no sessions to lose");
        Harness::try_parse_from(["service", "uninstall", "--accept-runtime-loss"])
            .expect_err("uninstall keeps its own --stop-sessions rule");
    }

    #[test]
    fn refresh_path_parses_its_flags_and_takes_no_prefix() {
        assert!(matches!(
            parse(&["refresh-path"]),
            Action::RefreshPath {
                dry_run: false,
                accept_runtime_loss: false,
                json: false
            }
        ));
        let action = parse(&[
            "refresh-path",
            "--dry-run",
            "--accept-runtime-loss",
            "--json",
        ]);
        assert!(matches!(
            action,
            Action::RefreshPath {
                dry_run: true,
                accept_runtime_loss: true,
                json: true
            }
        ));
        assert!(action.wants_json());
        Harness::try_parse_from(["service", "refresh-path", "--prefix", "/p"])
            .expect_err("the prefix is the installed one");
    }

    fn refresh_report() -> report::RefreshPathReport {
        report::RefreshPathReport {
            dry_run: false,
            changed: true,
            applied: true,
            recorded: vec![PathBuf::from("/old")],
            discovered: vec![PathBuf::from("/new")],
            added: vec![PathBuf::from("/new")],
            removed: vec![PathBuf::from("/old")],
            shell_used: Some(PathBuf::from("/bin/zsh")),
            shell_defaulted: true,
            dropped: Vec::new(),
            daemon_restart: "restarted",
            daemon_started: false,
            workers_kept: Some(2),
            resumed: false,
            rolled_back: None,
            preflight: None,
            accepted_runtime_loss: false,
            warnings: vec!["2 unknown keys are kept".to_owned()],
        }
    }

    #[test]
    fn refresh_path_renders_the_difference_the_restart_and_what_a_dry_run_leaves() {
        let report = refresh_report();
        let text = render_refresh_path(&report);
        assert!(text.contains("recorded 1 directories"), "{text}");
        assert!(text.contains("+ /new") && text.contains("- /old"), "{text}");
        assert!(
            text.contains("daemon restarted with the new search path; 2 workers kept running"),
            "{text}"
        );
        assert!(!text.contains("next replacement"), "{text}");
        assert!(text.contains("(default: $SHELL is unset)"), "{text}");
        assert!(text.contains("warning: 2 unknown keys are kept"), "{text}");

        let started = render_refresh_path(&report::RefreshPathReport {
            daemon_started: true,
            ..report.clone()
        });
        assert!(
            started.contains("(it was not running and was started)"),
            "{started}"
        );

        let preview = render_refresh_path(&report::RefreshPathReport {
            dry_run: true,
            applied: false,
            daemon_restart: "would_restart",
            ..report.clone()
        });
        assert!(preview.contains("would record 1 directories"), "{preview}");
        assert!(
            preview.contains("dry run: service.toml and the daemon are unchanged"),
            "{preview}"
        );
        assert!(preview.contains("would restart the daemon"), "{preview}");

        let current = render_refresh_path(&report::RefreshPathReport {
            changed: false,
            applied: false,
            daemon_restart: "none",
            added: Vec::new(),
            removed: Vec::new(),
            discovered: report.recorded.clone(),
            ..report.clone()
        });
        assert!(current.contains("is current"), "{current}");
        let json = render_json(&report::RefreshPathReport {
            dry_run: true,
            changed: false,
            applied: false,
            daemon_restart: "none",
            ..report
        })
        .expect("json");
        assert!(json.contains("\"dry_run\": true"), "{json}");
        assert!(json.contains("\"daemon_restart\": \"none\""), "{json}");
    }

    #[test]
    fn lock_takes_the_whole_command_after_the_separator() {
        let Action::Lock { command } = parse(&["lock", "--", "sh", "-c", "exit 3", "--json"])
        else {
            panic!("lock parses");
        };
        assert_eq!(command, ["sh", "-c", "exit 3", "--json"]);
        assert!(!Action::Lock { command }.wants_json());
        Harness::try_parse_from(["service", "lock"]).expect_err("a command is required");
        Harness::try_parse_from(["service", "lock", "sh"]).expect_err("the command follows `--`");
    }

    #[test]
    fn check_renders_the_transaction_and_its_pending_record() {
        let text = render_check(&report::CheckReport {
            operation: "install",
            version: "1.0.0".to_owned(),
            prefix: PathBuf::from("/p"),
            namespace: "ns".to_owned(),
            config_path: PathBuf::from("/c/pohunek/service.toml"),
            pending_transaction: Some(report::PendingReport {
                operation: "upgrade",
                version: "0.9.0".to_owned(),
                step: "config",
                rolling_back: false,
                preflight: None,
                accepted_runtime_loss: false,
                warnings: Vec::new(),
            }),
            pending_action: Some("roll_back"),
            locked: true,
            preflight: None,
            accepted_runtime_loss: false,
            acceptance: report::AcceptanceGroups::default(),
            config_schema_migration: None,
            warnings: Vec::new(),
        });
        assert!(
            text.contains("install of pohunek 1.0.0 would pass its preflight (namespace ns), checked under the transaction lock"),
            "{text}"
        );
        assert!(
            text.contains(
                "pending    upgrade 0.9.0 stopped after step config would be rolled back first"
            ),
            "{text}"
        );
    }

    /// A plan with one live and two stopped sessions; the refusals of a plan go to its error.
    fn runtime_plan_preflight() -> PreflightReport {
        use pohunek_service_config::preflight::{StoreReport, REPORT_VERSION};
        use pohunek_service_config::runtimes::{
            IntegrationAction, ProfileStatus, UseReason, ENSURE_REPORT_VERSION,
        };

        let session = |id: &str, verdict: Verdict| SessionVerdict {
            session_id: id.to_owned(),
            name: None,
            verdict,
            code: "native_recovery_unmappable".to_owned(),
            detail: "detail".to_owned(),
            acceptance_scope: None,
        };
        PreflightReport {
            report_version: REPORT_VERSION,
            daemon_version: "1.0.0".to_owned(),
            store: StoreReport {
                path: PathBuf::from("/d/metadata.db"),
                state: StoreState::UpToDate,
                schema_from: None,
                schema_to: 4,
                records: 3,
                error_code: None,
                error: None,
            },
            sessions: vec![session("s-live", Verdict::Adoptable)],
            stopped_sessions: vec![
                session("s-stopped-a", Verdict::WouldLoseRecovery),
                session("s-stopped-b", Verdict::WouldNotBeAdopted),
            ],
            unmanaged_workers: Vec::new(),
            profile_warnings: Vec::new(),
            runtimes: Some(EnsureReport {
                report_version: ENSURE_REPORT_VERSION,
                daemon_version: "1.0.0".to_owned(),
                dry_run: true,
                catalog: CatalogStatus::Current,
                packages: vec![PackageAction {
                    runtime: "claude".to_owned(),
                    package: "pohunek.runtime.claude".to_owned(),
                    version: "1.0.0".to_owned(),
                    digest: "de".repeat(32),
                    status: PackageStatus::Install,
                    reasons: vec![UseReason::Profile, UseReason::Session],
                    replaces_newer: None,
                    release_version: None,
                }],
                profiles: vec![ProfileAction {
                    name: "work".to_owned(),
                    base: "claude".to_owned(),
                    digest: "de".repeat(32),
                    status: ProfileStatus::Bind,
                    previous_digest: None,
                    previous_version: None,
                    version: Some("1.0.0".to_owned()),
                    kept_reason: None,
                }],
                integrations: vec![IntegrationAction {
                    runtime: "claude".to_owned(),
                    profile: None,
                }],
                refusals: Vec::new(),
                gaps: Vec::new(),
            }),
        }
    }

    #[test]
    fn check_renders_the_runtime_plan_and_every_stopped_session() {
        let mut preflight = runtime_plan_preflight();
        preflight.profile_warnings = vec![ProfileWarning {
            profile: "claude-norule".to_owned(),
            code: "profile_without_screen_readiness".to_owned(),
            detail: "profile claude-norule with manifest norule: the fix".to_owned(),
        }];
        let text = render_check(&report::CheckReport {
            operation: "upgrade",
            version: "1.0.0".to_owned(),
            prefix: PathBuf::from("/p"),
            namespace: "ns".to_owned(),
            config_path: PathBuf::from("/c/pohunek/service.toml"),
            pending_transaction: None,
            pending_action: None,
            locked: false,
            preflight: Some(preflight),
            accepted_runtime_loss: false,
            acceptance: report::AcceptanceGroups::default(),
            config_schema_migration: None,
            warnings: Vec::new(),
        });

        assert!(
            text.contains(
                "plan       runtime claude  pohunek.runtime.claude 1.0.0  install (profile, session)"
            ),
            "{text}"
        );
        assert!(
            text.contains("plan       profile work  pin to claude  bind"),
            "{text}"
        );
        assert!(
            text.contains("plan       integration claude  reinstall after the upgrade"),
            "{text}"
        );
        assert!(text.contains("catalog  current\n"), "{text}");
        assert!(text.contains("sessions   1 live, 1 adoptable\n"), "{text}");
        assert!(
            text.contains("stopped    2 with native resume or fork, 0 adoptable, 2 at risk\n"),
            "{text}"
        );
        assert!(
            text.contains("  s-stopped-a: would_lose_recovery"),
            "{text}"
        );
        assert!(
            text.contains(
                "profiles   1 with a warning, not blocking the upgrade\n  claude-norule: \
                 [profile_without_screen_readiness] profile claude-norule with manifest norule: the fix\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn check_counts_an_already_not_resumable_session_apart_from_the_at_risk_ones() {
        let session = |id: &str, verdict: Verdict| SessionVerdict {
            session_id: id.to_owned(),
            name: None,
            verdict,
            code: "test_code".to_owned(),
            detail: "test detail".to_owned(),
            acceptance_scope: None,
        };
        let mut preflight = runtime_plan_preflight();
        preflight.stopped_sessions = vec![
            session("s-stopped-a", Verdict::AlreadyNotResumable),
            session("s-stopped-b", Verdict::WouldLoseRecovery),
            session("s-stopped-c", Verdict::Adoptable),
        ];
        let text = render_check(&report::CheckReport {
            operation: "upgrade",
            version: "1.0.0".to_owned(),
            prefix: PathBuf::from("/p"),
            namespace: "ns".to_owned(),
            config_path: PathBuf::from("/c/pohunek/service.toml"),
            pending_transaction: None,
            pending_action: None,
            locked: false,
            preflight: Some(preflight),
            accepted_runtime_loss: false,
            acceptance: report::AcceptanceGroups::default(),
            config_schema_migration: None,
            warnings: Vec::new(),
        });

        assert!(
            text.contains(
                "stopped    3 with native resume or fork, 1 adoptable, 1 at risk, 1 already not resumable before the upgrade\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  s-stopped-a: already_not_resumable"),
            "{text}"
        );
    }

    #[test]
    fn a_rolled_back_journal_without_a_job_path_says_so() {
        let text = render_rolled_back(&report::PendingReport {
            operation: "upgrade",
            version: "2.0.0".to_owned(),
            step: "registered",
            rolling_back: false,
            preflight: None,
            accepted_runtime_loss: false,
            warnings: vec![
                "the journal /state/service-install.json records no job PATH".to_owned(),
            ],
        });
        assert!(
            text.contains("warning: the journal /state/service-install.json records no job PATH"),
            "{text}"
        );
    }

    #[test]
    fn status_renders_not_installed_and_pending_transactions() {
        let text = render_status(&report::StatusReport {
            installed: false,
            config_path: PathBuf::from("/c/pohunek/service.toml"),
            namespace: None,
            prefix: None,
            active_version: None,
            daemon: None,
            daemon_error: None,
            versions: Vec::new(),
            workers: Vec::new(),
            workers_error: None,
            unreadable_journals: Vec::new(),
            transaction_in_progress: false,
            warnings: Vec::new(),
            pending_transaction: Some(report::PendingReport {
                operation: "install",
                version: "1.0.0".to_owned(),
                step: "config",
                rolling_back: false,
                preflight: None,
                accepted_runtime_loss: false,
                warnings: Vec::new(),
            }),
        });
        assert!(text.contains("pending    install 1.0.0 stopped after step config"));
        assert!(text.contains("not installed (/c/pohunek/service.toml is missing)"));
    }

    #[test]
    fn status_names_only_refresh_path_as_the_way_out_of_a_pending_refresh() {
        let status = |operation| {
            render_status(&report::StatusReport {
                installed: false,
                config_path: PathBuf::from("/c/pohunek/service.toml"),
                namespace: None,
                prefix: None,
                active_version: None,
                daemon: None,
                daemon_error: None,
                versions: Vec::new(),
                workers: Vec::new(),
                workers_error: None,
                unreadable_journals: Vec::new(),
                transaction_in_progress: false,
                warnings: Vec::new(),
                pending_transaction: Some(report::PendingReport {
                    operation,
                    version: "1.0.0".to_owned(),
                    step: "config",
                    rolling_back: false,
                    preflight: None,
                    accepted_runtime_loss: false,
                    warnings: Vec::new(),
                }),
            })
        };
        let refresh = status("refresh_path");
        assert!(
            refresh.contains(
                "(rerun `pohunek service refresh-path` with the same `pohunek` (the release's `./pohunek` if you used it))"
            ),
            "{refresh}"
        );
        assert!(!refresh.contains("uninstall"), "{refresh}");
        let install = status("install");
        assert!(
            install.contains("(rerun it to resume, or uninstall to roll back)"),
            "{install}"
        );
    }

    #[test]
    fn a_finished_rollback_renders_no_difference_and_names_the_daemon_restart() {
        let rolled_back = report::RefreshPathReport {
            applied: false,
            daemon_restart: "restarted",
            rolled_back: Some(report::PendingReport {
                operation: "refresh_path",
                version: "1.0.0".to_owned(),
                step: "registered",
                rolling_back: true,
                preflight: None,
                accepted_runtime_loss: false,
                warnings: Vec::new(),
            }),
            ..refresh_report()
        };
        let text = render_refresh_path(&rolled_back);
        assert!(
            text.contains("rolled back an interrupted refresh_path"),
            "{text}"
        );
        assert!(
            text.contains("daemon restarted with the previous search path; 2 workers kept running"),
            "{text}"
        );
        assert!(
            !text.contains("would record")
                && !text.contains("recorded 1")
                && !text.contains("unchanged")
                && !text.contains("+ /new"),
            "{text}"
        );

        let untouched = render_refresh_path(&report::RefreshPathReport {
            daemon_restart: "none",
            ..rolled_back
        });
        assert!(
            untouched.contains("the daemon was not touched"),
            "{untouched}"
        );
        assert!(!untouched.contains("unchanged"), "{untouched}");
    }

    #[test]
    fn a_kept_package_and_an_unavailable_catalog_are_rendered_with_their_reason() {
        let runtimes = EnsureReport {
            report_version: pohunek_service_config::runtimes::ENSURE_REPORT_VERSION,
            daemon_version: "2.0.0".to_owned(),
            dry_run: true,
            packages: vec![pohunek_service_config::runtimes::PackageAction {
                runtime: "claude".to_owned(),
                package: "pohunek.runtime.claude".to_owned(),
                version: "1.0.0".to_owned(),
                digest: "de".repeat(32),
                status: PackageStatus::Kept,
                reasons: vec![pohunek_service_config::runtimes::UseReason::Profile],
                replaces_newer: None,
                release_version: None,
            }],
            catalog: CatalogStatus::Unavailable,
            profiles: Vec::new(),
            integrations: Vec::new(),
            refusals: Vec::new(),
            gaps: Vec::new(),
        };
        let mut text = String::new();
        render_runtimes(&mut text, &runtimes);
        assert!(
            text.contains(
                "catalog  unavailable: the release has no usable runtime catalog (absent, not verifiable, or older than the recorded one); the installed package keeps serving claude"
            ),
            "{text}"
        );
        assert!(
            text.contains("runtime claude  pohunek.runtime.claude 1.0.0  kept"),
            "{text}"
        );
    }

    #[test]
    fn an_unchanged_upgrade_renders_its_version_cleanup() {
        let text = render_upgrade(&report::UpgradeReport {
            warnings: Vec::new(),
            from_version: "2.0.0".to_owned(),
            to_version: "2.0.0".to_owned(),
            unchanged: true,
            resumed: false,
            rolled_back: None,
            removed_versions: vec!["1.0.0".to_owned()],
            kept_versions: vec![report::KeptVersion {
                version: "1.5.0".to_owned(),
                reason: "a worker may still run it".to_owned(),
            }],
            gc_error: Some("journals unreadable".to_owned()),
            preflight: None,
            accepted_runtime_loss: false,
            acceptance: report::AcceptanceGroups::default(),
            config_schema_migration: None,
            runtimes: None,
            integrations: Vec::new(),
            repair: None,
            refused_entries: Vec::new(),
        });
        assert!(text.contains("pohunek 2.0.0 is already active"), "{text}");
        assert!(!text.contains("upgraded"), "{text}");
        assert!(text.contains("removed version 1.0.0"), "{text}");
        assert!(
            text.contains("kept version 1.5.0: a worker may still run it"),
            "{text}"
        );
        assert!(
            text.contains("warning: old versions were not cleaned up: journals unreadable"),
            "{text}"
        );
    }

    /// `packaging/install-daemon.sh` detects a pending install by matching
    /// the pretty `"operation": "install"` pair, which only
    /// `pending_transaction` may carry.
    #[test]
    fn status_json_names_only_the_pending_operation() {
        let observation = report::JobReport {
            service_id: "pohunek-ns-daemon.service".to_owned(),
            state: "running",
            pid: Some(7),
            executable: Some(PathBuf::from("/p/libexec/pohunek/1.0.0/pohunekd")),
            arguments: Some(vec!["--service-config".to_owned()]),
        };
        let mut status = report::StatusReport {
            installed: true,
            config_path: PathBuf::from("/c/pohunek/service.toml"),
            namespace: Some("ns".to_owned()),
            prefix: Some(PathBuf::from("/p")),
            active_version: Some("1.0.0".to_owned()),
            daemon: Some(observation.clone()),
            daemon_error: Some("operation failed".to_owned()),
            versions: Vec::new(),
            workers: vec![report::WorkerReport {
                job: observation,
                session_id: "s-1".to_owned(),
                generation: "abcd2345".to_owned(),
                version: Some("1.0.0".to_owned()),
            }],
            workers_error: None,
            unreadable_journals: Vec::new(),
            transaction_in_progress: false,
            warnings: Vec::new(),
            pending_transaction: Some(report::PendingReport {
                operation: "install",
                version: "1.0.0".to_owned(),
                step: "ready",
                rolling_back: false,
                preflight: None,
                accepted_runtime_loss: false,
                warnings: Vec::new(),
            }),
        };
        let json = render_json(&status).expect("status JSON");
        assert_eq!(json.matches("\"operation\"").count(), 1, "{json}");
        assert!(json.contains("\"operation\": \"install\""), "{json}");

        status.pending_transaction = None;
        let json = render_json(&status).expect("status JSON");
        assert!(!json.contains("\"operation\""), "{json}");
        assert!(json.contains("\"pending_transaction\": null"), "{json}");
    }
}
