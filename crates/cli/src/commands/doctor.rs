//! `pohunek doctor` — environment health checks.
//!
//! Per `docs/plan-phase-1.md` "CLI Grammar": check agent binaries, git,
//! socket-dir perms, and state-dir writability. (Schema-version check is part of
//! the `SQLite` milestone and is therefore reported as not-yet-available rather
//! than faked.)
//!
//! Exit status: non-zero if any *required* check fails. Agent binaries are
//! reported but their absence is a warning, not a hard failure, because a user
//! may run only one of the two agents.
//!
//! The probe list is platform specific (see the `hostcheck` crate). On macOS
//! the CLI adds the `launchd_job` check, which reads the installed service's
//! state through `pohunek service status`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use hostcheck::{AccessDir, Platform, StandardCheckInputs, WorkerCandidate};
use protocol::{DoctorCheck, DoctorReport as Report, DoctorStatus as Status};

use crate::client::Client;
use crate::commands::render_json;
use crate::error::CliError;
use crate::paths::Paths;
use crate::service::report::StatusReport;

// Rust guideline compliant 2026-09-30

/// Environment variable overriding the worker executable path.
///
/// Mirrors the override the daemon honors when it is not installed as a
/// native service.
const WORKER_BIN_ENV: &str = "POHUNEK_WORKER_BIN";

/// `launchd_job` state of a daemon job that is up.
const JOB_RUNNING: &str = "running";

/// `launchd_job` state of a daemon job the service manager gave up on.
const JOB_FAILED: &str = "failed";

/// Run `doctor`. Returns `true` if the environment is healthy (no fatal checks).
///
/// # Errors
///
/// Only returns an error if paths cannot be resolved at all; individual failed
/// checks are reported in the output, not returned as errors.
pub(crate) async fn run(paths: &Paths, json: bool) -> Result<bool, CliError> {
    let launcher_bin_dir = paths.launcher_bin_dir();
    let sway_config_dir = paths.sway_config_dir();
    let home_dir = std::env::var_os(pohunek_paths::HOME).map(PathBuf::from);
    let worker = worker_candidate(&paths.config_dir);
    let working_dir = std::env::current_dir().ok();
    let access_dirs: Vec<AccessDir<'_>> = working_dir
        .iter()
        .map(|path| AccessDir {
            label: "current directory",
            path,
        })
        .collect();
    let mut checks = hostcheck::standard_checks(StandardCheckInputs {
        socket_dir: &paths.runtime_dir,
        state_dir: &paths.data_dir,
        log_dir: &paths.log_dir,
        launcher_bin_dir: &launcher_bin_dir,
        sway_config_dir: &sway_config_dir,
        config_dir: &paths.config_dir,
        home_dir: home_dir.as_deref(),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
        worker: worker.as_ref(),
        access_dirs: &access_dirs,
    });
    if hostcheck::current_platform() == Platform::MacOs {
        checks.push(launchd_job_check(crate::service::status().await));
    }

    match Client::connect("local", paths).await {
        Ok(mut client) => match client.daemon_doctor().await {
            Ok(remote) => merge_daemon_checks(&mut checks, remote.checks),
            Err(_error) => checks.push(unavailable_daemon_check()),
        },
        Err(_error) => checks.push(unavailable_daemon_check()),
    }
    let report = Report::from_checks(checks);

    if json {
        print!("{}", render_json(&report)?);
    } else {
        print_human(&report);
    }

    Ok(report.overall != Status::Fail)
}

/// The worker executable the daemon would launch on this host.
///
/// An installed `service.toml` names the versioned worker; otherwise the
/// `POHUNEK_WORKER_BIN` override, then `pohunek-sessiond` next to this
/// executable. A `service.toml` that cannot be loaded is left to
/// `pohunek service status`, which reports it.
fn worker_candidate(config_dir: &Path) -> Option<WorkerCandidate> {
    let executable = std::env::current_exe().ok();
    worker_candidate_from(
        config_dir,
        std::env::var_os(WORKER_BIN_ENV),
        executable.as_deref().and_then(Path::parent),
    )
}

/// [`worker_candidate`] with the environment override and executable directory
/// passed in.
fn worker_candidate_from(
    config_dir: &Path,
    env_override: Option<OsString>,
    executable_dir: Option<&Path>,
) -> Option<WorkerCandidate> {
    let service_worker = pohunek_service_config::ServiceConfig::load(
        &config_dir.join(pohunek_service_config::FILE_NAME),
    )
    .ok()
    .map(|config| config.worker_executable());
    hostcheck::resolve_worker_candidate(service_worker, env_override, executable_dir)
}

/// Map the native service status to the `launchd_job` check.
///
/// A missing service is a `warn` (a manually started daemon is valid); only a
/// job the service manager reports as failed is a `fail`.
fn launchd_job_check(status: Result<StatusReport, crate::service::Error>) -> DoctorCheck {
    const NAME: &str = "launchd_job";
    let report = match status {
        Ok(report) => report,
        Err(error) => {
            return DoctorCheck::new(
                NAME,
                Status::Warn,
                format!("service status is unavailable: {error}; run 'pohunek service status'"),
            );
        }
    };
    if !report.installed {
        return DoctorCheck::new(
            NAME,
            Status::Warn,
            "native service is not installed; run 'pohunek service install' to supervise \
             sessions with launchd",
        );
    }
    if report.pending_transaction.is_some() {
        return DoctorCheck::new(
            NAME,
            Status::Warn,
            "a service install or upgrade is pending; rerun it to completion",
        );
    }
    if let Some(error) = report.daemon_error {
        return DoctorCheck::new(
            NAME,
            Status::Warn,
            format!("daemon job state is unavailable: {error}"),
        );
    }
    match report.daemon {
        None => DoctorCheck::new(
            NAME,
            Status::Warn,
            "the daemon job is not registered with launchd; run 'pohunek service upgrade'",
        ),
        Some(job) if job.state == JOB_RUNNING => DoctorCheck::new(
            NAME,
            Status::Ok,
            format!("daemon job {} is running", job.service_id),
        ),
        Some(job) if job.state == JOB_FAILED => DoctorCheck::new(
            NAME,
            Status::Fail,
            format!(
                "daemon job {} failed; inspect the launchd log under the state directory and \
                 run 'pohunek service status'",
                job.service_id
            ),
        ),
        Some(job) => DoctorCheck::new(
            NAME,
            Status::Warn,
            format!("daemon job {} is {}", job.service_id, job.state),
        ),
    }
}

fn merge_daemon_checks(checks: &mut Vec<DoctorCheck>, remote: Vec<DoctorCheck>) {
    for check in remote {
        if !checks.iter().any(|existing| existing.name == check.name) {
            checks.push(check);
        }
    }
}

fn unavailable_daemon_check() -> DoctorCheck {
    DoctorCheck::new(
        "host_governance_inspection",
        Status::Warn,
        "Daemon governance inspection is unavailable; start the local daemon and retry.",
    )
}

/// Render the report as an aligned human table.
fn print_human(report: &Report) {
    let width = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0)
        .max("CHECK".len());

    println!("{:<width$}  STATUS  DETAIL", "CHECK", width = width);
    for c in &report.checks {
        println!(
            "{:<width$}  {:<6}  {}",
            c.name,
            c.status.as_str(),
            c.detail,
            width = width
        );
    }
    println!();
    println!("overall: {}", report.overall.as_str());
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// Per-test unique temp dir, namespaced by pid + a monotonic counter so
    /// parallel tests never collide.
    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("pohunek-doctor-test-{pid}-{n}"))
    }

    /// Build a `Paths` rooted at `base` (same-crate `pub(crate)` fields).
    fn paths_at(base: &Path) -> Paths {
        Paths {
            runtime_dir: base.join("runtime"),
            socket: base.join("runtime").join("daemon.sock"),
            data_dir: base.join("data"),
            log_dir: base.join("logs"),
            cache_dir: base.join("cache"),
            config_home: base.join("config"),
            config_dir: base.join("config").join("pohunek"),
        }
    }

    /// The CLI doctor wiring assembles a report (the individual host probes are
    /// covered in the `hostcheck` crate). Writable temp dirs make the
    /// directory-writability checks pass; the result is rendered as JSON without
    /// error.
    #[tokio::test]
    async fn run_assembles_report_without_error() {
        let base = unique_temp_dir();
        let paths = paths_at(&base);

        let healthy = run(&paths, true).await.expect("doctor run resolves");
        // We assert only that the run completed and produced a boolean verdict;
        // the exact value depends on which binaries exist on the test host.
        let _ = healthy;

        let _ = std::fs::remove_dir_all(&base);
    }

    fn job(state: &'static str) -> crate::service::report::JobReport {
        crate::service::report::JobReport {
            service_id: "dev.pohunek.daemon".to_owned(),
            state,
            pid: None,
            executable: None,
            arguments: None,
        }
    }

    fn status(installed: bool, daemon: Option<crate::service::report::JobReport>) -> StatusReport {
        StatusReport {
            installed,
            config_path: PathBuf::from("/cfg/service.toml"),
            namespace: None,
            prefix: None,
            active_version: None,
            daemon,
            daemon_error: None,
            versions: Vec::new(),
            workers: Vec::new(),
            workers_error: None,
            unreadable_journals: Vec::new(),
            pending_transaction: None,
            transaction_in_progress: false,
        }
    }

    #[test]
    fn launchd_job_check_is_fatal_only_for_a_failed_job() {
        let run = |report| launchd_job_check(Ok(report)).status;

        assert_eq!(run(status(true, Some(job("running")))), Status::Ok);
        assert_eq!(run(status(true, Some(job("failed")))), Status::Fail);
        assert_eq!(run(status(true, Some(job("stopped")))), Status::Warn);
        assert_eq!(run(status(true, Some(job("starting")))), Status::Warn);
        assert_eq!(run(status(true, None)), Status::Warn);
        assert_eq!(run(status(false, None)), Status::Warn);
    }

    #[test]
    fn launchd_job_check_reports_daemon_errors_and_pending_transactions_as_warnings() {
        let mut with_error = status(true, Some(job("running")));
        with_error.daemon_error = Some("launchctl exited with status 5".to_owned());
        let check = launchd_job_check(Ok(with_error));
        assert_eq!(check.name, "launchd_job");
        assert_eq!(check.status, Status::Warn);

        let mut pending = status(true, Some(job("running")));
        pending.pending_transaction = Some(crate::service::report::PendingReport {
            operation: "upgrade",
            version: "1.0.0".to_owned(),
            step: "bootstrap",
        });
        assert_eq!(launchd_job_check(Ok(pending)).status, Status::Warn);
    }

    #[test]
    fn launchd_job_check_warns_when_status_is_unavailable() {
        let error = crate::service::Error::NotInstalled {
            path: PathBuf::from("/cfg/service.toml"),
        };
        let check = launchd_job_check(Err(error));
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("pohunek service status"));
    }

    #[test]
    fn worker_candidate_without_service_config_prefers_the_override_then_the_sibling() {
        let base = unique_temp_dir();
        std::fs::create_dir_all(&base).expect("create config dir");
        let exe_dir = Path::new("/opt/pohunek/bin");

        let overridden = worker_candidate_from(
            &base,
            Some(OsString::from("/custom/pohunek-sessiond")),
            Some(exe_dir),
        )
        .expect("override candidate");
        assert_eq!(overridden.source, hostcheck::WorkerSource::Environment);

        let sibling = worker_candidate_from(&base, None, Some(exe_dir)).expect("sibling candidate");
        assert_eq!(sibling.source, hostcheck::WorkerSource::Sibling);
        assert_eq!(sibling.path, exe_dir.join("pohunek-sessiond"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worker_candidate_ignores_an_unloadable_service_config() {
        let base = unique_temp_dir();
        std::fs::create_dir_all(&base).expect("create config dir");
        std::fs::write(base.join("service.toml"), "not = [valid").expect("write service.toml");

        let candidate = worker_candidate_from(&base, None, Some(Path::new("/opt/pohunek/bin")))
            .expect("falls back to the sibling");

        assert_eq!(candidate.source, hostcheck::WorkerSource::Sibling);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_does_not_duplicate_local_host_checks() {
        let mut checks = vec![DoctorCheck::new("state_dir_writable", Status::Ok, "local")];
        merge_daemon_checks(
            &mut checks,
            vec![
                DoctorCheck::new("state_dir_writable", Status::Fail, "remote"),
                DoctorCheck::new("host_identity_stable", Status::Ok, "remote"),
            ],
        );
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].detail, "local");
        assert_eq!(checks[1].name, "host_identity_stable");
    }

    #[test]
    fn unavailable_daemon_check_is_redacted_warning() {
        let check = unavailable_daemon_check();
        assert_eq!(check.status, Status::Warn);
        assert_eq!(check.name, "host_governance_inspection");
        assert!(!check.detail.contains('/'));
        assert!(!check.detail.contains("socket"));
    }
}
