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

use hostcheck::{AccessDir, Platform, StandardCheckInputs, Supervision, WorkerCandidate};
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
    let home_dir = std::env::var_os(pohunek_paths::HOME).map(PathBuf::from);
    let worker = worker_candidate(&paths.config_dir);
    let supervision = supervision_mode(&paths.config_dir);
    let working_dir = std::env::current_dir();
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
        config_dir: &paths.config_dir,
        home_dir: home_dir.as_deref(),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
        worker: worker.as_ref(),
        access_dirs: &access_dirs,
        supervision,
    });
    let platform = hostcheck::current_platform();
    checks.extend(working_directory_check(platform, &working_dir));
    let job_status = if platform == Platform::MacOs {
        Some(crate::service::status().await)
    } else {
        None
    };

    // The daemon's bounded probes (a wedged `launchctl` waits up to
    // `PROBE_BUDGET`) must be able to finish before the client gives up.
    let (connected, remote) = match Client::connect_with_request_timeout(
        "local",
        paths,
        hostcheck::doctor_request_timeout(),
    )
    .await
    {
        Ok(mut client) => (
            true,
            client
                .daemon_doctor()
                .await
                .ok()
                .map(|report| report.checks),
        ),
        Err(_error) => (false, None),
    };
    if let Some(job_status) = job_status {
        // Reachability is the control connection, not the doctor RPC: a daemon
        // that accepts the connection but fails or times out `daemon.doctor`
        // still answers.
        checks.push(launchd_job_check(job_status, connected, supervision));
    }
    match remote {
        Some(remote) => {
            reconcile_launchd_checks(&mut checks, &remote);
            merge_daemon_checks(&mut checks, remote);
        }
        None => checks.push(unavailable_daemon_check()),
    }
    let report = Report::from_checks(checks);

    if json {
        print!("{}", render_json(&report)?);
    } else {
        print_human(&report);
    }

    Ok(report.overall != Status::Fail)
}

/// Report a current directory that cannot be determined.
///
/// The `filesystem_access` probe reads the current directory on macOS, where
/// projects live below privacy-protected folders; a removed or unreadable
/// directory must not silently skip that probe. Other platforms do not probe it,
/// so they report nothing.
fn working_directory_check(
    platform: Platform,
    working_dir: &std::io::Result<PathBuf>,
) -> Option<DoctorCheck> {
    if platform != Platform::MacOs {
        return None;
    }
    let error = working_dir.as_ref().err()?;
    Some(DoctorCheck::new(
        "working_directory",
        Status::Fail,
        format!(
            "the current directory cannot be determined: {error}; run 'pohunek doctor' from an \
             existing directory you can read"
        ),
    ))
}

/// The supervision mode this process can infer for the daemon.
///
/// An installed `service.toml` means `pohunek daemon start` runs the native
/// service; without it the daemon may be a `--dev-subprocess` one or not yet
/// installed, which the CLI cannot tell apart.
fn supervision_mode(config_dir: &Path) -> Supervision {
    if config_dir.join(pohunek_service_config::FILE_NAME).exists() {
        Supervision::Native
    } else {
        Supervision::Unknown
    }
}

/// The worker executable the daemon would launch on this host.
///
/// An installed `service.toml` names the versioned worker; otherwise the
/// `POHUNEK_WORKER_BIN` override, then `pohunek-sessiond` next to the `pohunekd`
/// that `pohunek daemon start` would run ([`locate_daemon`]). A `service.toml`
/// that cannot be loaded is left to `pohunek service status`, which reports it.
///
/// [`locate_daemon`]: crate::commands::daemon::locate_daemon
fn worker_candidate(config_dir: &Path) -> Option<WorkerCandidate> {
    let daemon = crate::commands::daemon::locate_daemon().ok();
    worker_candidate_from(
        config_dir,
        std::env::var_os(WORKER_BIN_ENV),
        daemon.as_deref().and_then(Path::parent),
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
/// Every applicable finding is reported and the worst status wins, so a failed
/// daemon job is never hidden by a pending transaction or an unreadable job
/// state. A missing service is a `warn` (a manually started daemon is valid).
///
/// The launchd backend never reports a failed job: a loaded label is `running`
/// (its process exists) or `unknown` (no process), and launchd records no exit
/// for it. A loaded job without a process whose daemon also does not answer
/// the doctor's control connection (`daemon_reachable`) is therefore the
/// failure signal, next to a backend-reported `failed` state (systemd).
pub(crate) fn launchd_job_check(
    status: Result<StatusReport, crate::service::Error>,
    daemon_reachable: bool,
    supervision: Supervision,
) -> DoctorCheck {
    hostcheck::apply_supervision(supervision, launchd_job_findings(status, daemon_reachable))
}

fn launchd_job_findings(
    status: Result<StatusReport, crate::service::Error>,
    daemon_reachable: bool,
) -> DoctorCheck {
    const NAME: &str = LAUNCHD_JOB_CHECK;
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
    let mut findings: Vec<(Status, String)> = Vec::new();
    if !report.installed {
        findings.push((
            Status::Warn,
            "native service is not installed; run 'pohunek service install' to supervise \
             sessions with launchd"
                .to_owned(),
        ));
    }
    if report.pending_transaction.is_some() {
        findings.push((
            Status::Warn,
            "a service install or upgrade is pending; rerun it to completion".to_owned(),
        ));
    }
    if let Some(error) = &report.daemon_error {
        findings.push((
            Status::Warn,
            format!("daemon job state is unavailable: {error}"),
        ));
    }
    if report.installed && report.daemon_error.is_none() {
        findings.push(match &report.daemon {
            None => (
                Status::Warn,
                "the daemon job is not registered with launchd; run 'pohunek service upgrade'"
                    .to_owned(),
            ),
            Some(job) if job.state == JOB_RUNNING => (
                Status::Ok,
                format!("daemon job {} is running", job.service_id),
            ),
            Some(job) if job.state == JOB_FAILED => (
                Status::Fail,
                format!(
                    "daemon job {} failed; inspect the launchd log under the state directory and \
                     run 'pohunek service status'",
                    job.service_id
                ),
            ),
            Some(job) if daemon_reachable => (
                Status::Warn,
                format!(
                    "daemon job {} is {} but a daemon answers on the control socket",
                    job.service_id, job.state
                ),
            ),
            Some(job) => (
                Status::Fail,
                format!(
                    "daemon job {} is loaded but not running ({}) and the daemon does not answer \
                     on the control socket; inspect the launchd log under the state directory and \
                     run 'pohunek service status'",
                    job.service_id, job.state
                ),
            ),
        });
    }
    let overall = findings
        .iter()
        .fold(Status::Ok, |overall, (status, _)| worse(overall, *status));
    let detail = findings
        .into_iter()
        .map(|(_, detail)| detail)
        .collect::<Vec<_>>()
        .join("; ");
    DoctorCheck::new(NAME, overall, detail)
}

/// Name of the CLI-only daemon job check.
const LAUNCHD_JOB_CHECK: &str = "launchd_job";

/// Drop the local launchd checks when the daemon that answered does not run
/// under launchd.
///
/// The daemon runs the same platform list with its real supervision, so a
/// `--dev-subprocess` daemon reports none of the launchd checks even when a
/// `service.toml` is installed and the CLI inferred a native service. A native
/// daemon reports them, and both results are then merged.
fn reconcile_launchd_checks(checks: &mut Vec<DoctorCheck>, remote: &[DoctorCheck]) {
    let daemon_native = remote
        .iter()
        .any(|check| hostcheck::LAUNCHD_CHECKS.contains(&check.name.as_str()));
    if !daemon_native {
        checks.retain(|check| {
            check.name != LAUNCHD_JOB_CHECK
                && !hostcheck::LAUNCHD_CHECKS.contains(&check.name.as_str())
        });
    }
}

/// Add the daemon's checks to the local ones.
///
/// Both processes probe the same list, but from different contexts: a
/// terminal-launched CLI and a launchd-launched daemon differ in `PATH`,
/// privacy grants and working directory. A name collision therefore keeps both
/// results in one check: the worse status wins, and the details are joined as
/// `local: ...; daemon: ...` unless they are identical. Human and JSON output
/// render the same merged list.
///
/// The worker executable is the exception: the daemon knows the worker its
/// active supervision launches (a `--dev-subprocess` or `--service-config`
/// daemon may differ from what the CLI derives), so its result replaces the
/// CLI's derivation.
fn merge_daemon_checks(checks: &mut Vec<DoctorCheck>, remote: Vec<DoctorCheck>) {
    for check in remote {
        match checks
            .iter_mut()
            .find(|existing| existing.name == check.name)
        {
            Some(existing) if check.name == hostcheck::WORKER_EXECUTABLE_CHECK => *existing = check,
            Some(existing) => merge_collision(existing, &check),
            None => checks.push(check),
        }
    }
}

/// Fold a daemon result into the local check of the same name.
fn merge_collision(local: &mut DoctorCheck, remote: &DoctorCheck) {
    if local.detail != remote.detail {
        local.detail = format!("local: {}; daemon: {}", local.detail, remote.detail);
    }
    local.status = worse(local.status, remote.status);
}

/// The more severe of two statuses (`fail` over `warn` over `ok`).
fn worse(left: Status, right: Status) -> Status {
    match (left, right) {
        (Status::Fail, _) | (_, Status::Fail) => Status::Fail,
        (Status::Warn, _) | (_, Status::Warn) => Status::Warn,
        (Status::Ok, Status::Ok) => Status::Ok,
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
    use super::*;

    /// A private fixture directory guard and a not-yet-created child path of
    /// it; the guard removes the directory when it drops, also when a test fails.
    fn unique_temp_dir() -> (tempfile::TempDir, PathBuf) {
        let guard = pohunek_test_support::tempdir().expect("create fixture directory");
        let base = guard.path().join("base");
        (guard, base)
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
    fn launchd_job_check_is_fatal_for_a_failed_or_unreachable_unstarted_job() {
        let run = |report| launchd_job_check(Ok(report), true, Supervision::Native).status;
        let run_unreachable =
            |report| launchd_job_check(Ok(report), false, Supervision::Native).status;

        assert_eq!(run(status(true, Some(job("running")))), Status::Ok);
        assert_eq!(run(status(true, Some(job("failed")))), Status::Fail);
        assert_eq!(run(status(true, Some(job("stopped")))), Status::Warn);
        assert_eq!(run(status(true, Some(job("starting")))), Status::Warn);
        assert_eq!(run(status(true, None)), Status::Warn);
        assert_eq!(run(status(false, None)), Status::Warn);

        // The macOS backend reports a loaded job without a process as `unknown`:
        // fatal only when no daemon answers either.
        assert_eq!(run(status(true, Some(job("unknown")))), Status::Warn);
        assert_eq!(
            run_unreachable(status(true, Some(job("unknown")))),
            Status::Fail
        );
        assert_eq!(
            run_unreachable(status(true, Some(job("running")))),
            Status::Ok
        );
        assert_eq!(run_unreachable(status(false, None)), Status::Warn);
    }

    #[test]
    fn launchd_job_check_reports_daemon_errors_and_pending_transactions_as_warnings() {
        let mut with_error = status(true, Some(job("running")));
        with_error.daemon_error = Some("launchctl exited with status 5".to_owned());
        let check = launchd_job_check(Ok(with_error), false, Supervision::Native);
        assert_eq!(check.name, "launchd_job");
        assert_eq!(check.status, Status::Warn);

        let mut pending = status(true, Some(job("running")));
        pending.pending_transaction = Some(crate::service::report::PendingReport {
            operation: "upgrade",
            version: "1.0.0".to_owned(),
            step: "bootstrap",
        });
        assert_eq!(
            launchd_job_check(Ok(pending), false, Supervision::Native).status,
            Status::Warn
        );
    }

    #[test]
    fn a_subprocess_daemon_removes_the_local_launchd_checks() {
        // An installed service.toml made the CLI infer a native service, but the
        // answering daemon is a `--dev-subprocess` one on a host without a
        // `gui/<uid>` domain (for example over SSH).
        let local = || {
            vec![
                DoctorCheck::new("bin:git", Status::Ok, "found"),
                DoctorCheck::new("launchd_domain", Status::Fail, "no gui domain"),
                DoctorCheck::new("launchd_agents_dir", Status::Warn, "x"),
                DoctorCheck::new("launchd_job", Status::Fail, "not running"),
            ]
        };
        let subprocess_daemon = vec![DoctorCheck::new("bin:git", Status::Ok, "found")];

        let mut checks = local();
        reconcile_launchd_checks(&mut checks, &subprocess_daemon);
        merge_daemon_checks(&mut checks, subprocess_daemon);
        assert_eq!(checks.len(), 1);
        assert_ne!(Report::from_checks(checks).overall, Status::Fail);

        // A native daemon reports the launchd checks itself: keep and merge.
        let native_daemon = vec![DoctorCheck::new("launchd_domain", Status::Ok, "reachable")];
        let mut checks = local();
        reconcile_launchd_checks(&mut checks, &native_daemon);
        merge_daemon_checks(&mut checks, native_daemon);
        assert_eq!(checks.len(), 4);
        let domain = checks.iter().find(|c| c.name == "launchd_domain").unwrap();
        assert_eq!(domain.status, Status::Fail, "the worse result wins");
    }

    #[test]
    fn an_unknown_supervision_mode_softens_a_job_failure() {
        let report = || status(true, Some(job("unknown")));

        let native = launchd_job_check(Ok(report()), false, Supervision::Native);
        let unknown = launchd_job_check(Ok(report()), false, Supervision::Unknown);

        assert_eq!(native.status, Status::Fail);
        assert_eq!(unknown.status, Status::Warn);
        assert!(unknown.detail.contains("supervision mode unknown"));
    }

    #[test]
    fn a_failed_job_is_reported_even_with_a_pending_transaction_or_state_error() {
        let pending = || crate::service::report::PendingReport {
            operation: "upgrade",
            version: "1.0.0".to_owned(),
            step: "bootstrap",
        };
        let mut failed = status(true, Some(job("failed")));
        failed.pending_transaction = Some(pending());

        let check = launchd_job_check(Ok(failed), false, Supervision::Native);

        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("failed"), "{}", check.detail);
        assert!(check.detail.contains("pending"), "{}", check.detail);

        // A running job with a pending transaction stays a warning and says both.
        let mut running = status(true, Some(job("running")));
        running.pending_transaction = Some(pending());
        let check = launchd_job_check(Ok(running), false, Supervision::Native);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("is running"), "{}", check.detail);
        assert!(check.detail.contains("pending"), "{}", check.detail);
    }

    #[test]
    fn launchd_job_check_warns_when_status_is_unavailable() {
        let error = crate::service::Error::NotInstalled {
            path: PathBuf::from("/cfg/service.toml"),
        };
        let check = launchd_job_check(Err(error), false, Supervision::Native);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("pohunek service status"));
    }

    #[test]
    fn worker_candidate_without_service_config_prefers_the_override_then_the_sibling() {
        let (_guard, base) = unique_temp_dir();
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
    }

    #[test]
    fn worker_candidate_ignores_an_unloadable_service_config() {
        let (_guard, base) = unique_temp_dir();
        std::fs::create_dir_all(&base).expect("create config dir");
        std::fs::write(base.join("service.toml"), "not = [valid").expect("write service.toml");

        let candidate = worker_candidate_from(&base, None, Some(Path::new("/opt/pohunek/bin")))
            .expect("falls back to the sibling");

        assert_eq!(candidate.source, hostcheck::WorkerSource::Sibling);
    }

    #[test]
    fn the_daemons_worker_check_replaces_the_local_derivation() {
        let mut checks = vec![
            DoctorCheck::new("worker_executable", Status::Fail, "derived beside the CLI"),
            DoctorCheck::new("state_dir_writable", Status::Ok, "local"),
        ];

        merge_daemon_checks(
            &mut checks,
            vec![
                DoctorCheck::new("worker_executable", Status::Ok, "active supervision"),
                DoctorCheck::new("state_dir_writable", Status::Fail, "remote"),
            ],
        );

        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].status, Status::Ok);
        assert_eq!(checks[0].detail, "active supervision");
        assert_eq!(checks[1].status, Status::Fail, "other collisions merge");
        assert_eq!(checks[1].detail, "local: local; daemon: remote");
    }

    #[test]
    fn the_daemon_doctor_call_outlasts_the_client_default_and_the_probe_budget() {
        let timeout = hostcheck::doctor_request_timeout();

        assert!(timeout > hostcheck::PROBE_BUDGET);
        assert!(
            timeout > pohunek_client::ClientOptions::default().request_timeout,
            "the client's default would drop the reply of a slow bounded probe"
        );
    }

    #[test]
    fn a_daemon_failure_is_never_hidden_by_a_local_ok() {
        let mut checks = vec![DoctorCheck::new("state_dir_writable", Status::Ok, "local")];
        merge_daemon_checks(
            &mut checks,
            vec![
                DoctorCheck::new("state_dir_writable", Status::Fail, "remote"),
                DoctorCheck::new("host_identity_stable", Status::Ok, "remote"),
            ],
        );

        assert_eq!(checks.len(), 2, "one merged check per name");
        assert_eq!(checks[0].name, "state_dir_writable");
        assert_eq!(checks[0].status, Status::Fail);
        assert_eq!(checks[0].detail, "local: local; daemon: remote");
        assert_eq!(checks[1].name, "host_identity_stable");
        assert_eq!(Report::from_checks(checks).overall, Status::Fail);
    }

    #[test]
    fn identical_results_collapse_and_the_worse_status_wins_either_way() {
        let mut checks = vec![
            DoctorCheck::new("bin:git", Status::Ok, "found at /usr/bin/git"),
            DoctorCheck::new("filesystem_access", Status::Fail, "denied"),
            DoctorCheck::new("bin:codex", Status::Warn, "missing"),
        ];
        merge_daemon_checks(
            &mut checks,
            vec![
                DoctorCheck::new("bin:git", Status::Ok, "found at /usr/bin/git"),
                DoctorCheck::new("filesystem_access", Status::Ok, "readable"),
                DoctorCheck::new("bin:codex", Status::Ok, "found"),
            ],
        );

        assert_eq!(checks[0].detail, "found at /usr/bin/git");
        assert_eq!(checks[0].status, Status::Ok);
        assert_eq!(checks[1].status, Status::Fail);
        assert!(checks[1].detail.contains("local: denied; daemon: readable"));
        assert_eq!(checks[2].status, Status::Warn);
    }

    #[test]
    fn an_unreadable_working_directory_is_an_explicit_macos_failure() {
        let error: std::io::Result<PathBuf> = Err(std::io::Error::from_raw_os_error(2));

        let check = working_directory_check(Platform::MacOs, &error).expect("a failing check");
        assert_eq!(check.name, "working_directory");
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("existing directory"),
            "{}",
            check.detail
        );

        assert!(working_directory_check(Platform::Linux, &error).is_none());
        let ok: std::io::Result<PathBuf> = Ok(PathBuf::from("/work"));
        assert!(working_directory_check(Platform::MacOs, &ok).is_none());
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
