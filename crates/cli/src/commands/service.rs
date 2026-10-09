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

use clap::{Subcommand, ValueHint};
use pohunek_service_config::preflight::{PreflightReport, StoreState};

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
        /// Upgrade even if live sessions would lose their runtime or native
        /// recovery.
        ///
        /// The new daemon's adoption preflight lists every live session it
        /// would not adopt or would adopt without native recovery, and the
        /// upgrade refuses them without this flag. It never overrides a
        /// metadata store the new daemon cannot start with.
        #[arg(long)]
        accept_runtime_loss: bool,
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
        /// Pass the check although live sessions would lose their runtime or
        /// native recovery, as `service upgrade --accept-runtime-loss` would.
        #[arg(long)]
        accept_runtime_loss: bool,
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
        Action::Install { from, prefix, json } => {
            let report = service::install(from, prefix).await?;
            emit(json, &report, render_install)
        }
        Action::Upgrade {
            from,
            accept_runtime_loss,
            json,
        } => {
            let report = service::upgrade(from, accept_runtime_loss).await?;
            emit(json, &report, render_upgrade)
        }
        Action::Check {
            prefix,
            accept_runtime_loss,
            json,
        } => {
            let report = service::check(prefix, accept_runtime_loss).await?;
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
/// live session with its verdict, reason code and evidence.
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
        return;
    }
    let at_risk = preflight.at_risk().count();
    let _ = writeln!(
        text,
        "sessions   {} live, {} adoptable{}",
        preflight.sessions.len(),
        preflight.sessions.len() - at_risk,
        if at_risk == 0 {
            String::new()
        } else if accepted_runtime_loss {
            format!(", {at_risk} at risk (accepted with --accept-runtime-loss)")
        } else {
            format!(", {at_risk} at risk")
        }
    );
    for session in &preflight.sessions {
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

fn render_upgrade(report: &report::UpgradeReport) -> String {
    let mut text = String::new();
    if let Some(pending) = &report.rolled_back {
        let _ = writeln!(text, "{}", render_rolled_back(pending));
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
    for version in &report.removed_versions {
        let _ = writeln!(text, "removed version {version}");
    }
    for kept in &report.kept_versions {
        let _ = writeln!(text, "kept version {}: {}", kept.version, kept.reason);
    }
    if let Some(error) = &report.gc_error {
        let _ = writeln!(text, "warning: old versions were not cleaned up: {error}");
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
        render_preflight(&mut text, preflight, report.accepted_runtime_loss);
    }
    text
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
    }
    if let Some(preflight) = &report.preflight {
        render_preflight(&mut text, preflight, report.accepted_runtime_loss);
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
    let mut text = format!(
        "rolled back an interrupted {} of {} (stopped after step {})",
        pending.operation, pending.version, pending.step
    );
    if let Some(preflight) = &pending.preflight {
        text.push('\n');
        render_preflight(&mut text, preflight, pending.accepted_runtime_loss);
    }
    text.trim_end().to_owned()
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
            Action::Install { from: Some(from), prefix: Some(prefix), json: true }
                if from == Path::new("/a") && prefix == Path::new("/p")
        ));
        assert!(matches!(
            parse(&["upgrade", "--from", "/a"]),
            Action::Upgrade {
                from: Some(_),
                accept_runtime_loss: false,
                json: false
            }
        ));
        assert!(matches!(
            parse(&["upgrade", "--accept-runtime-loss", "--json"]),
            Action::Upgrade {
                from: None,
                accept_runtime_loss: true,
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
            Action::Check { prefix: Some(prefix), json: true, accept_runtime_loss: false }
                if prefix == Path::new("/p")
        ));
        assert!(matches!(
            parse(&["check"]),
            Action::Check {
                prefix: None,
                accept_runtime_loss: false,
                json: false
            }
        ));
        assert!(matches!(
            parse(&["check", "--accept-runtime-loss"]),
            Action::Check {
                accept_runtime_loss: true,
                ..
            }
        ));
        Harness::try_parse_from(["service", "install", "--accept-runtime-loss"])
            .expect_err("install has no sessions to lose");
        Harness::try_parse_from(["service", "uninstall", "--accept-runtime-loss"])
            .expect_err("uninstall keeps its own --stop-sessions rule");
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
                preflight: None,
                accepted_runtime_loss: false,
            }),
            pending_action: Some("roll_back"),
            locked: true,
            preflight: None,
            accepted_runtime_loss: false,
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
                preflight: None,
                accepted_runtime_loss: false,
            }),
        });
        assert!(text.contains("pending    install 1.0.0 stopped after step config"));
        assert!(text.contains("not installed (/c/pohunek/service.toml is missing)"));
    }

    #[test]
    fn an_unchanged_upgrade_renders_its_version_cleanup() {
        let text = render_upgrade(&report::UpgradeReport {
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
            pending_transaction: Some(report::PendingReport {
                operation: "install",
                version: "1.0.0".to_owned(),
                step: "ready",
                preflight: None,
                accepted_runtime_loss: false,
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
