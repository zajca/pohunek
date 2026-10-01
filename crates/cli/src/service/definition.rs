//! `service.toml` contents and the daemon's job definition.

// Rust guideline compliant 2026-09-30

use std::path::Path;

use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::{JobDefinition, JobSpec, RestartPolicy, SEARCH_PATH_VARIABLE};
use pohunek_service_config::{ConfigSpec, Deadlines, ServiceConfig};
use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;

use super::context::Context;
use super::error::{supervisor_error, Error};
use super::report::SearchPathReport;
use super::settings;

/// Daemon argument naming the service configuration file.
const SERVICE_CONFIG_FLAG: &str = "--service-config";

/// Builds the configuration a fresh install writes.
///
/// Every value comes from a documented constant in
/// [`settings`](super::settings); the namespace inputs are the effective UID
/// and the canonical application roots. The daemon `PATH` is resolved by
/// [`Context::install_search_path`], which may run the user's login shell.
///
/// # Errors
///
/// Returns a filesystem error when a root is unsafe,
/// [`Error::SearchPath`] when no `PATH` can be resolved, and
/// [`Error::Config`] when a value fails validation.
pub fn initial_config(
    context: &Context,
    prefix: &Path,
    version: &str,
) -> Result<ServiceConfig, Error> {
    initial_config_reported(context, prefix, version).map(|(config, _report)| config)
}

/// Like [`initial_config`], and also returns how the `PATH` was resolved.
///
/// # Errors
///
/// See [`initial_config`].
pub fn initial_config_reported(
    context: &Context,
    prefix: &Path,
    version: &str,
) -> Result<(ServiceConfig, SearchPathReport), Error> {
    let (search_path, report) = context.install_search_path()?;
    let config = config_with_search_path(context, prefix, version, search_path)?;
    Ok((config, report))
}

/// Builds the configuration of an installation that is only identified, not
/// installed, such as when an interrupted install is uninstalled.
///
/// The search path stays empty: no daemon starts from this value, so no login
/// shell runs.
///
/// # Errors
///
/// Returns a filesystem error when a root is unsafe and [`Error::Config`]
/// when a value fails validation.
pub fn identity_config(
    context: &Context,
    prefix: &Path,
    version: &str,
) -> Result<ServiceConfig, Error> {
    config_with_search_path(context, prefix, version, SearchPath::empty())
}

pub(crate) fn config_with_search_path(
    context: &Context,
    prefix: &Path,
    version: &str,
    search_path: SearchPath,
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
        search_path,
        sweep_grace: settings::SWEEP_GRACE,
        open_files: settings::OPEN_FILES,
    })?)
}

/// Returns `config` with only its active version changed.
///
/// The recorded search path is kept, so an upgrade never re-runs discovery.
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
/// and receives the bootstrap XDG environment plus the recorded `PATH` when
/// [`ServiceConfig::search_path`] is not empty. On macOS launchd writes its
/// stdout and stderr below `<log_dir>/launchd`.
///
/// # Errors
///
/// Returns [`Error::NonUtf8Env`] for a bootstrap root the definition cannot
/// carry and [`Error::Supervisor`] when the definition fails validation.
pub fn daemon_definition(
    context: &Context,
    config: &ServiceConfig,
) -> Result<JobDefinition, Error> {
    let config_path = context.config_path();
    let deadlines = config.deadlines();
    let mut environment = context.bootstrap_environment()?;
    if !config.search_path().is_empty() {
        environment.insert(
            SEARCH_PATH_VARIABLE.to_owned(),
            config.search_path().to_env_value(),
        );
    }
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
        environment,
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
    use crate::service::context::managed_path_discovery;
    use crate::service::context::tests::temp_root;
    use crate::service::context::tests::{context, make_dirs};
    use crate::service::context::PathDiscovery;

    #[test]
    fn initial_config_writes_the_documented_values() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        let prefix = root.as_path().join("prefix");
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
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        let config =
            initial_config(&context, &root.as_path().join("prefix"), "1.2.3").expect("config");
        let definition = daemon_definition(&context, &config).expect("definition");
        assert_eq!(
            definition.executable(),
            root.as_path().join("prefix/libexec/pohunek/1.2.3/pohunekd")
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
        assert_eq!(
            definition.environment(),
            &context.bootstrap_environment().expect("utf-8 roots")
        );
        assert_eq!(definition.working_directory(), config.state_root());
        assert_eq!(definition.open_files(), settings::OPEN_FILES);
    }

    /// Writes an executable fake login shell and returns its path.
    fn fake_shell(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("fake-shell");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write shell");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shell");
        path
    }

    /// A context whose discovery runs `shell` and falls back to `fallback`.
    fn managed_context(root: &Path, shell: std::path::PathBuf, fallback: &[String]) -> Context {
        let discovery = managed_path_discovery(Some(shell), Vec::new()).expect("discovery");
        let PathDiscovery::Managed {
            login_shell: Some(mut login_shell),
            ..
        } = discovery
        else {
            panic!("managed discovery");
        };
        login_shell.timeout = std::time::Duration::from_millis(500);
        context(root).with_path_discovery(PathDiscovery::Managed {
            login_shell: Some(login_shell),
            shell_defaulted: false,
            fallback_directories: fallback.to_vec(),
        })
    }

    #[test]
    fn an_unmanaged_context_records_no_path() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        let config =
            initial_config(&context, &root.as_path().join("prefix"), "1.2.3").expect("config");
        assert!(config.search_path().is_empty());
        let definition = daemon_definition(&context, &config).expect("definition");
        assert!(!definition.environment().contains_key("PATH"));
    }

    #[test]
    fn the_login_shell_path_reaches_the_daemon_job_and_service_toml() {
        let (_root, root) = temp_root();
        let root = root.as_path();
        // Apple Silicon Homebrew, a user-local install, and directories with
        // spaces, quotes, and Unicode.
        let brew = root.join("opt/homebrew/bin");
        let local = root.join("home/.local/bin");
        let project = root.join("Projekty \u{10d}esk\u{e9}/it's \"a\" app/bin");
        for dir in [&brew, &local, &project] {
            make_dirs(root, dir);
        }
        let value = format!(
            "{}:{}:{}",
            brew.display(),
            local.display(),
            project.display()
        );
        let value_file = root.join("path-value");
        std::fs::write(&value_file, &value).expect("value");
        let shell = fake_shell(
            root,
            &format!(
                "echo 'banner from .zshrc'\nPATH=\"$(cat '{}')\"; export PATH\nexec /bin/sh -c \"$3\"",
                value_file.display()
            ),
        );
        let context = managed_context(root, shell, &[]);
        let config = initial_config(&context, &root.join("prefix"), "1.2.3").expect("config");
        assert_eq!(config.search_path().to_env_value(), value);
        let definition = daemon_definition(&context, &config).expect("definition");
        assert_eq!(definition.environment().get("PATH"), Some(&value));
        // The XDG roots stay alongside the PATH entry.
        assert!(definition.environment().contains_key("HOME"));

        let written = root.join("service.toml");
        config.write(&written).expect("write service.toml");
        let loaded = ServiceConfig::load(&written).expect("load service.toml");
        assert_eq!(loaded.search_path(), config.search_path());
        let upgraded = with_version(&loaded, "1.2.4").expect("upgrade");
        assert_eq!(upgraded.search_path(), config.search_path());
        assert_eq!(
            daemon_definition(&context, &upgraded)
                .expect("definition")
                .environment()
                .get("PATH"),
            Some(&value)
        );
    }

    #[test]
    fn the_probe_sees_shell_so_a_profile_branching_on_it_sets_its_path() {
        let (_root, root) = temp_root();
        let root = root.as_path();
        let prefix = root.join("opt/tools/bin");
        make_dirs(root, &prefix);
        // A profile that sets `PATH` only when `$SHELL` names this very shell.
        let path = root.join("fake-shell");
        let shell = fake_shell(
            root,
            &format!(
                "[ \"$SHELL\" = '{}' ] && PATH='{}'; export PATH\nexec /bin/sh -c \"$3\"",
                path.display(),
                prefix.display()
            ),
        );
        assert_eq!(shell, path);
        let context = managed_context(root, shell, &[]);
        let (recorded, report) = context.install_search_path().expect("resolve");
        assert_eq!(recorded.entries(), [prefix]);
        assert_eq!(report.source, "login_shell");
    }

    #[test]
    fn a_broken_login_shell_falls_back_to_the_directory_list() {
        let (_root, root) = temp_root();
        let root = root.as_path();
        let brew = root.join("opt/homebrew/bin");
        let cargo = root.join("home/.cargo/bin");
        for dir in [&brew, &cargo] {
            make_dirs(root, dir);
        }
        let fallback = vec![
            "~/.cargo/bin".to_owned(),
            brew.display().to_string(),
            root.join("usr/local/bin").display().to_string(),
        ];
        for body in [
            "exit 1",
            "echo junk before the sentinel",
            "exit 0",
            "exec sleep 300",
        ] {
            let shell = fake_shell(root, body);
            let context = managed_context(root, shell, &fallback);
            let config = initial_config(&context, &root.join("prefix"), "1.2.3").expect(body);
            assert_eq!(
                config.search_path().entries(),
                [cargo.clone(), brew.clone()],
                "{body}"
            );
        }
    }

    #[test]
    fn no_usable_directory_fails_the_install_instead_of_inventing_a_path() {
        let (_root, root) = temp_root();
        let root = root.as_path();
        let shell = fake_shell(root, "exit 1");
        let context = managed_context(root, shell, &[root.join("missing").display().to_string()]);
        let error =
            initial_config(&context, &root.join("prefix"), "1.2.3").expect_err("no directory");
        assert_eq!(error.code(), "service_search_path_unavailable");
    }

    #[test]
    fn identity_config_never_runs_the_login_shell() {
        let (_root, root) = temp_root();
        let root = root.as_path();
        let marker = root.join("ran");
        let shell = fake_shell(root, &format!("touch '{}'", marker.display()));
        let context = managed_context(root, shell, &[]);
        let config =
            identity_config(&context, &root.join("prefix"), "1.2.3").expect("identity config");
        assert!(config.search_path().is_empty());
        assert!(!marker.exists());
    }

    #[test]
    fn the_managed_policy_uses_the_documented_defaults() {
        let PathDiscovery::Managed {
            login_shell: Some(spec),
            shell_defaulted,
            fallback_directories,
        } = managed_path_discovery(None, Vec::new()).expect("discovery")
        else {
            panic!("managed discovery");
        };
        assert!(shell_defaulted, "an unset $SHELL is reported as defaulted");
        assert_eq!(spec.shell, Path::new(settings::DEFAULT_LOGIN_SHELL));
        assert_eq!(spec.timeout, settings::LOGIN_SHELL_TIMEOUT);
        assert_eq!(spec.max_output_bytes, settings::LOGIN_SHELL_OUTPUT);
        assert!(fallback_directories.contains(&"/opt/homebrew/bin".to_owned()));
        assert!(fallback_directories.contains(&"/usr/local/bin".to_owned()));
        let PathDiscovery::Managed {
            login_shell: Some(spec),
            shell_defaulted,
            ..
        } = managed_path_discovery(Some(std::path::PathBuf::from("/bin/fish")), Vec::new())
            .expect("discovery")
        else {
            panic!("managed discovery");
        };
        assert!(!shell_defaulted);
        assert_eq!(spec.shell, Path::new("/bin/fish"));
    }

    #[test]
    fn a_relative_shell_is_refused_instead_of_replaced() {
        for shell in ["relative/zsh", "zsh", ""] {
            let error = managed_path_discovery(Some(std::path::PathBuf::from(shell)), Vec::new())
                .expect_err(shell);
            assert_eq!(error.code(), "service_environment_invalid", "{shell:?}");
        }
    }

    #[test]
    fn the_report_names_the_source_the_failure_and_refused_directories() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_root, root) = temp_root();
        let root = root.as_path();
        let good = root.join("home/.local/bin");
        let loose = root.join("opt/homebrew/bin");
        make_dirs(root, &good);
        make_dirs(root, &loose);
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o775)).expect("chmod");
        let fallback = vec!["~/.local/bin".to_owned(), loose.display().to_string()];
        let value = format!("{}:{}", good.display(), loose.display());
        let value_file = root.join("path-value");
        std::fs::write(&value_file, &value).expect("value");

        // A successful probe: its untrusted directory is refused and reported.
        let shell = fake_shell(
            root,
            &format!(
                "PATH=\"$(cat '{}')\"; export PATH\nexec /bin/sh -c \"$3\"",
                value_file.display()
            ),
        );
        let context = managed_context(root, shell, &fallback);
        let (path, report) = context.install_search_path().expect("resolve");
        assert_eq!(path.entries(), std::slice::from_ref(&good));
        assert_eq!(report.source, "login_shell");
        assert!(report.login_shell_failure.is_none());
        assert_eq!(report.dropped.len(), 1, "{:?}", report.dropped);
        assert_eq!(report.dropped[0].path, loose.display().to_string());
        assert!(report.needs_warning());

        // A failing probe: the fallback list is used and the reason is reported.
        let shell = fake_shell(root, "exit 3");
        let context = managed_context(root, shell.clone(), &fallback);
        let (path, report) = context.install_search_path().expect("resolve");
        assert_eq!(path.entries(), [good]);
        assert_eq!(report.source, "fallback");
        assert_eq!(report.shell_used.as_deref(), Some(shell.as_path()));
        assert!(!report.shell_defaulted);
        let failure = report.login_shell_failure.expect("failure");
        assert!(failure.contains("exit code 3"), "{failure}");
    }
}
