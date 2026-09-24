//! `pohunek service` — install, upgrade, uninstall, and inspect the native service.
//!
//! The clap surface and human rendering live here; the transactions live in
//! [`crate::service`]. Every subcommand is local to this machine and ignores
//! the global `--host`.

// Rust guideline compliant 2026-09-24

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Subcommand, ValueHint};

use crate::commands::render_json;
use crate::error::CliError;
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
        /// host identity.
        #[arg(long)]
        purge: bool,
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
}

impl Action {
    /// Whether the subcommand requested `--json` output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::Install { json, .. }
            | Self::Upgrade { json, .. }
            | Self::Uninstall { json, .. }
            | Self::Status { json } => *json,
        }
    }
}

/// Runs one `pohunek service` subcommand.
///
/// # Errors
///
/// Returns [`CliError::Service`] when the operation fails.
pub(crate) async fn run(action: Action) -> Result<(), CliError> {
    match action {
        Action::Install { from, prefix, json } => {
            let report = service::install(from, prefix).await?;
            emit(json, &report, render_install)
        }
        Action::Upgrade { from, json } => {
            let report = service::upgrade(from).await?;
            emit(json, &report, render_upgrade)
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
    }
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
    text
}

fn render_upgrade(report: &report::UpgradeReport) -> String {
    let mut text = String::new();
    if let Some(pending) = &report.rolled_back {
        let _ = writeln!(text, "{}", render_rolled_back(pending));
    }
    if report.unchanged {
        let _ = writeln!(text, "pohunek {} is already active", report.to_version);
        return text;
    }
    let _ = writeln!(
        text,
        "upgraded pohunek {} -> {}{}",
        report.from_version,
        report.to_version,
        if report.resumed { ", resumed" } else { "" }
    );
    for version in &report.removed_versions {
        let _ = writeln!(text, "removed version {version}");
    }
    for kept in &report.kept_versions {
        let _ = writeln!(text, "kept version {}: {}", kept.version, kept.reason);
    }
    if let Some(error) = &report.gc_error {
        let _ = writeln!(text, "warning: old versions were not cleaned up: {error}");
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
    for kept in &report.kept_versions {
        let _ = writeln!(text, "kept version {}: {}", kept.version, kept.reason);
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
    text
}

fn render_transaction(text: &mut String, report: &report::StatusReport) {
    match (&report.pending_transaction, report.transaction_in_progress) {
        (Some(pending), true) => {
            let _ = writeln!(
                text,
                "running    {} {} is at step {} in another `pohunek service` command",
                pending.operation, pending.version, pending.step
            );
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
                "pending    {} {} stopped after step {} (rerun it to resume, or uninstall to roll back)",
                pending.operation, pending.version, pending.step
            );
        }
        (None, false) => {}
    }
}

fn render_rolled_back(pending: &report::PendingReport) -> String {
    format!(
        "rolled back an interrupted {} of {} (stopped after step {})",
        pending.operation, pending.version, pending.step
    )
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn every_subcommand_and_flag_parses() {
        assert!(matches!(
            parse(&["install", "--from", "/a", "--prefix", "/p", "--json"]),
            Action::Install { from: Some(from), prefix: Some(prefix), json: true }
                if from == Path::new("/a") && prefix == Path::new("/p")
        ));
        assert!(matches!(
            parse(&["upgrade", "--from", "/a"]),
            Action::Upgrade {
                from: Some(_),
                json: false
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
            pending_transaction: Some(report::PendingReport {
                operation: "install",
                version: "1.0.0".to_owned(),
                step: "config",
            }),
        });
        assert!(text.contains("pending    install 1.0.0 stopped after step config"));
        assert!(text.contains("not installed (/c/pohunek/service.toml is missing)"));
    }
}
