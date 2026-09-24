//! `service.toml` contents and the daemon's job definition.

// Rust guideline compliant 2026-09-24

use std::path::Path;

use pohunek_platform::supervisor::{JobDefinition, JobSpec, RestartPolicy};
use pohunek_service_config::{ConfigSpec, Deadlines, ServiceConfig};
use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;

use super::context::Context;
use super::error::{supervisor_error, Error};
use super::settings;

/// Daemon argument naming the service configuration file.
const SERVICE_CONFIG_FLAG: &str = "--service-config";

/// Builds the configuration a fresh install writes.
///
/// Every value comes from a documented constant in
/// [`settings`](super::settings); the namespace inputs are the effective UID
/// and the canonical application roots.
///
/// # Errors
///
/// Returns a filesystem error when a root is unsafe and
/// [`Error::Config`] when a value fails validation.
pub fn initial_config(
    context: &Context,
    prefix: &Path,
    version: &str,
) -> Result<ServiceConfig, Error> {
    let (state_root, runtime_root) = context.roots()?;
    Ok(ServiceConfig::new(ConfigSpec {
        prefix: prefix.to_path_buf(),
        active_version: version.to_owned(),
        uid: context.uid(),
        state_root,
        runtime_root,
        deadlines: Deadlines {
            worker_connect: settings::WORKER_CONNECT,
            worker_initialize: settings::WORKER_INITIALIZE,
            launchctl_command: settings::LAUNCHCTL_COMMAND,
            worker_exit_timeout: settings::WORKER_EXIT_TIMEOUT,
            daemon_exit_timeout: settings::DAEMON_EXIT_TIMEOUT,
            daemon_restart_throttle: settings::DAEMON_RESTART_THROTTLE,
        },
        environment_allowlist: DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
        sweep_grace: settings::SWEEP_GRACE,
        open_files: settings::OPEN_FILES,
    })?)
}

/// Returns `config` with only its active version changed.
///
/// # Errors
///
/// Returns [`Error::Config`] for an invalid version.
pub fn with_version(config: &ServiceConfig, version: &str) -> Result<ServiceConfig, Error> {
    let mut spec = config.to_spec();
    version.clone_into(&mut spec.active_version);
    Ok(ServiceConfig::new(spec)?)
}

/// Builds the daemon's job definition for `config`.
///
/// The daemon runs the versioned `pohunekd --service-config <file>`, restarts
/// only after a failure (throttled), starts in the application state root,
/// and receives the bootstrap XDG environment. On macOS launchd writes its
/// stdout and stderr below `<log_dir>/launchd`.
///
/// # Errors
///
/// Returns [`Error::Supervisor`] when the definition fails validation.
pub fn daemon_definition(
    context: &Context,
    config: &ServiceConfig,
) -> Result<JobDefinition, Error> {
    let config_path = context.config_path();
    let deadlines = config.deadlines();
    JobDefinition::new(JobSpec {
        executable: config.daemon_executable(),
        arguments: vec![
            SERVICE_CONFIG_FLAG.to_owned(),
            config_path
                .to_str()
                .ok_or_else(|| Error::InvalidPath {
                    flag: "service.toml path",
                    path: config_path.clone(),
                })?
                .to_owned(),
        ],
        environment: context.bootstrap_environment(),
        working_directory: config.state_root().to_path_buf(),
        logs: daemon_logs(context, config),
        start_timeout: settings::DAEMON_START_TIMEOUT,
        exit_timeout: deadlines.daemon_exit_timeout,
        restart: RestartPolicy::OnFailure {
            throttle: deadlines.daemon_restart_throttle,
        },
        open_files: config.open_files(),
    })
    .map_err(|source| supervisor_error("build daemon definition", source))
}

#[cfg(target_os = "macos")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature for both targets; systemd keeps no log files"
)]
fn daemon_logs(
    context: &Context,
    config: &ServiceConfig,
) -> Option<pohunek_platform::supervisor::JobLogs> {
    let label = config.namespace().daemon_label();
    let directory = context.paths().launchd_log_dir();
    Some(pohunek_platform::supervisor::JobLogs {
        stdout: directory.join(format!("{label}.out.log")),
        stderr: directory.join(format!("{label}.err.log")),
    })
}

/// systemd sends daemon stdio to the user journal; no log files exist.
#[cfg(not(target_os = "macos"))]
fn daemon_logs(
    _context: &Context,
    _config: &ServiceConfig,
) -> Option<pohunek_platform::supervisor::JobLogs> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::context::tests::context;

    #[test]
    fn initial_config_writes_the_documented_values() {
        let root = tempfile::tempdir().expect("temp dir");
        let context = context(root.path());
        let prefix = root.path().join("prefix");
        let config = initial_config(&context, &prefix, "1.2.3").expect("config");
        assert_eq!(config.active_version(), "1.2.3");
        assert_eq!(config.prefix(), prefix);
        let deadlines = config.deadlines();
        assert_eq!(deadlines.worker_connect.as_secs(), 10);
        assert_eq!(deadlines.worker_initialize.as_secs(), 45);
        assert_eq!(deadlines.launchctl_command.as_secs(), 10);
        assert_eq!(deadlines.worker_exit_timeout.as_secs(), 30);
        assert_eq!(deadlines.daemon_exit_timeout.as_secs(), 30);
        assert_eq!(deadlines.daemon_restart_throttle.as_secs(), 5);
        assert_eq!(config.sweep_grace().as_secs(), 5);
        assert_eq!(config.open_files(), 8_192);
        assert_eq!(
            config.environment_allowlist(),
            DEFAULT_ENVIRONMENT_ALLOWLIST
        );
        assert_eq!(config.namespace(), context.namespace().expect("namespace"));
        assert_eq!(
            with_version(&config, "1.2.4")
                .expect("upgrade config")
                .active_version(),
            "1.2.4"
        );
    }

    #[test]
    fn daemon_definition_runs_the_versioned_daemon_with_the_config() {
        let root = tempfile::tempdir().expect("temp dir");
        let context = context(root.path());
        let config =
            initial_config(&context, &root.path().join("prefix"), "1.2.3").expect("config");
        let definition = daemon_definition(&context, &config).expect("definition");
        assert_eq!(
            definition.executable(),
            root.path().join("prefix/libexec/pohunek/1.2.3/pohunekd")
        );
        assert_eq!(
            definition.arguments(),
            [
                "--service-config".to_owned(),
                context.config_path().to_str().expect("utf-8").to_owned()
            ]
        );
        assert_eq!(
            definition.restart(),
            RestartPolicy::OnFailure {
                throttle: settings::DAEMON_RESTART_THROTTLE
            }
        );
        assert_eq!(definition.environment(), &context.bootstrap_environment());
        assert_eq!(definition.working_directory(), config.state_root());
        assert_eq!(definition.open_files(), settings::OPEN_FILES);
    }
}
