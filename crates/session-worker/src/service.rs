//! Loads and verifies the service configuration a supervised worker starts with.
//!
//! A worker started by systemd or launchd receives `--service-config <path>`
//! naming the installation's `service.toml`. Before binding its socket the
//! worker parses that file with [`ServiceConfig::load`], which also enforces
//! the owner-private `0600` trust rules, and then proves that the file belongs
//! to this installation and this binary:
//!
//! - [`ServiceConfig::verify_installation`] compares the recorded user and
//!   canonical state and runtime roots with the worker's own, so a file written
//!   for another user or installation is refused;
//! - the running executable must be the session worker of *some* version
//!   directory below the recorded prefix. It is deliberately not required to be
//!   the active version: an upgrade rewrites `active_version` while live
//!   workers keep running, and until the daemon restarts it may still start
//!   generations from the version it runs.

// Rust guideline compliant 2026-09-24

use std::path::{Path, PathBuf};

use pohunek_paths::BasePaths;
use pohunek_service_config::{ConfigError, ServiceConfig};
use tracing::{event, Level};

/// Reports why a worker refuses its service configuration.
#[derive(Debug, thiserror::Error)]
pub enum ServiceConfigError {
    /// The file is untrusted, unreadable, malformed, or records another installation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The running worker is not an installed session worker of this prefix.
    #[error(
        "worker executable {} is not a session worker below {} of the service configuration",
        executable.display(),
        versions_dir.display()
    )]
    ForeignExecutable {
        /// Canonical path of the running executable.
        executable: PathBuf,
        /// Recorded `<prefix>/libexec/pohunek` directory.
        versions_dir: PathBuf,
    },
    /// A path needed for the executable comparison could not be resolved.
    #[error("cannot resolve {} for the service configuration check: {source}", path.display())]
    Resolve {
        /// Path that failed to resolve.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

/// Loads `path` and verifies it describes this installation and executable.
///
/// `uid` is the worker's effective user, `paths` its resolved application
/// directories, and `executable` the running worker binary.
///
/// # Errors
///
/// Returns [`ServiceConfigError::Config`] for every [`ServiceConfig::load`]
/// and [`ServiceConfig::verify_installation`] failure (missing, untrusted,
/// invalid, or foreign file), [`ServiceConfigError::ForeignExecutable`] when
/// `executable` is not `<prefix>/libexec/pohunek/<version>/pohunek-sessiond`,
/// and [`ServiceConfigError::Resolve`] when that comparison cannot resolve a
/// path.
pub fn load_service_config(
    path: &Path,
    uid: u32,
    paths: &BasePaths,
    executable: &Path,
) -> Result<ServiceConfig, ServiceConfigError> {
    let config = ServiceConfig::load(path)?;
    config.verify_installation(uid, &paths.state_dir, &paths.runtime_dir)?;
    verify_executable(&config, executable)?;
    Ok(config)
}

/// Logs each unknown key of `config` at debug level.
///
/// Every generation loads the same file, so the daemon alone warns about
/// unknown keys; repeating the warning on each spawn would flood the log. The
/// worker calls this after its logging is initialized, because a subscriber
/// installed later would not see the events.
pub fn log_unknown_service_keys(config: &ServiceConfig) {
    for key in config.unknown_keys() {
        event!(
            Level::DEBUG,
            event = "service_config.unknown_key",
            key.path = %key.path,
            key.line = key.line,
            "service.toml holds a key this version does not know; it is ignored"
        );
    }
}

/// Requires `executable` to be the worker binary of a recorded version directory.
fn verify_executable(config: &ServiceConfig, executable: &Path) -> Result<(), ServiceConfigError> {
    let executable = canonicalize(executable)?;
    let versions_dir = config.layout().versions_dir();
    let foreign = || ServiceConfigError::ForeignExecutable {
        executable: executable.clone(),
        versions_dir: versions_dir.clone(),
    };
    let version = executable
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(foreign)?;
    let expected = config
        .layout()
        .worker_executable(version)
        .ok_or_else(foreign)?;
    // The recorded prefix may traverse symlinks (for example a relocated
    // home); compare resolved paths so only the real file identity matters.
    match std::fs::canonicalize(&expected) {
        Ok(expected) if expected == executable => Ok(()),
        Ok(_) => Err(foreign()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(foreign()),
        Err(source) => Err(ServiceConfigError::Resolve {
            path: expected,
            source,
        }),
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, ServiceConfigError> {
    std::fs::canonicalize(path).map_err(|source| ServiceConfigError::Resolve {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use pohunek_paths::{PathEnv, Platform};
    use pohunek_service_config::{ConfigSpec, Deadlines};
    use pohunek_test_support::env::TestEnv;

    use super::*;

    const VERSION: &str = "1.2.3";
    const OLD_VERSION: &str = "1.2.2";

    struct Installation {
        _env: TestEnv,
        paths: BasePaths,
        config_path: PathBuf,
        prefix: PathBuf,
        uid: u32,
    }

    fn private_dir(path: &Path) {
        fs::create_dir_all(path).expect("create directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod");
    }

    fn install_worker(prefix: &Path, version: &str) -> PathBuf {
        let executable = prefix
            .join("libexec/pohunek")
            .join(version)
            .join("pohunek-sessiond");
        fs::create_dir_all(executable.parent().expect("version dir")).expect("version dir");
        fs::write(&executable, b"#!/bin/sh\n").expect("write worker");
        executable
    }

    fn installation() -> Installation {
        let test_env = TestEnv::new().expect("hermetic test environment");
        let os = |path: &Path| Some(path.as_os_str().to_owned());
        let paths = BasePaths::resolve_for(
            Platform::current().expect("platform"),
            rustix::process::geteuid().as_raw(),
            &PathEnv {
                xdg_runtime_dir: os(test_env.runtime_dir()),
                xdg_data_home: os(test_env.data_home()),
                xdg_state_home: os(test_env.state_home()),
                xdg_cache_home: os(test_env.cache_home()),
                xdg_config_home: os(test_env.config_home()),
                home: os(test_env.home()),
            },
        )
        .expect("resolve paths");
        for path in [&paths.runtime_dir, &paths.state_dir, &paths.config_dir] {
            private_dir(path);
        }
        let prefix = test_env.root().join("prefix");
        let uid = rustix::process::geteuid().as_raw();
        let deadline = Duration::from_secs(1);
        let config = ServiceConfig::new(ConfigSpec {
            prefix: prefix.clone(),
            active_version: VERSION.to_owned(),
            uid,
            state_root: fs::canonicalize(&paths.state_dir).expect("state root"),
            runtime_root: fs::canonicalize(&paths.runtime_dir).expect("runtime root"),
            deadlines: Deadlines {
                worker_connect: deadline,
                worker_initialize: deadline,
                launchctl_command: deadline,
                worker_exit_timeout: deadline,
                daemon_exit_timeout: deadline,
                daemon_restart_throttle: deadline,
            },
            environment_allowlist: vec!["PATH".to_owned(), "LC_*".to_owned()],
            search_path: pohunek_platform::shell_env::SearchPath::default(),
            sweep_grace: deadline,
            input_timing: pohunek_service_config::DEFAULT_INPUT_TIMING,
            wait_timing: pohunek_service_config::DEFAULT_WAIT_TIMING,
            hook_stderr_tail_bytes: pohunek_service_config::DEFAULT_HOOK_STDERR_TAIL_BYTES,
            event_retention: pohunek_service_config::DEFAULT_EVENT_RETENTION,
            open_files: 8192,
        })
        .expect("valid configuration");
        let config_path = pohunek_service_config::file_path(&paths);
        config.write(&config_path).expect("write configuration");
        Installation {
            _env: test_env,
            paths,
            config_path,
            prefix,
            uid,
        }
    }

    impl Installation {
        fn load(&self, executable: &Path) -> Result<ServiceConfig, ServiceConfigError> {
            load_service_config(&self.config_path, self.uid, &self.paths, executable)
        }
    }

    #[test]
    fn a_valid_configuration_for_this_installation_is_accepted() {
        let installation = installation();
        let executable = install_worker(&installation.prefix, VERSION);

        let config = installation.load(&executable).expect("valid configuration");

        assert_eq!(config.active_version(), VERSION);
        assert_eq!(config.environment_allowlist(), ["PATH", "LC_*"]);
    }

    /// Shared buffer a test subscriber writes its formatted events to.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log capture").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn a_worker_generation_loads_unknown_keys_quietly() {
        const SECRET: &str = "PH-WORKER-sentinel-6e02-91cd";
        let installation = installation();
        let executable = install_worker(&installation.prefix, VERSION);
        let text = fs::read_to_string(&installation.config_path).expect("read service.toml");
        let text = text.replacen(
            "[limits]\n",
            &format!("[limits]\nfuture_limit = \"{SECRET}\"\n"),
            1,
        );
        fs::write(&installation.config_path, text).expect("write service.toml");

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(Level::DEBUG)
            .finish();
        let config = tracing::subscriber::with_default(subscriber, || {
            let config = installation.load(&executable).expect("tolerant load");
            log_unknown_service_keys(&config);
            config
        });

        assert_eq!(config.unknown_keys().len(), 1);
        let logged = String::from_utf8_lossy(&capture.0.lock().expect("log capture")).into_owned();
        assert!(logged.contains("limits.future_limit"), "{logged}");
        assert!(logged.contains("DEBUG"), "{logged}");
        assert!(!logged.contains("WARN"), "{logged}");
        assert!(!logged.contains(SECRET), "{logged}");
    }

    #[test]
    fn a_worker_of_a_previous_version_is_accepted_after_upgrade() {
        let installation = installation();
        let executable = install_worker(&installation.prefix, OLD_VERSION);

        installation
            .load(&executable)
            .expect("installed previous version");
    }

    #[test]
    fn a_missing_or_untrusted_file_is_refused() {
        let installation = installation();
        let executable = install_worker(&installation.prefix, VERSION);

        fs::set_permissions(&installation.config_path, fs::Permissions::from_mode(0o644))
            .expect("chmod");
        assert!(matches!(
            installation.load(&executable),
            Err(ServiceConfigError::Config(ConfigError::Untrusted { .. }))
        ));

        fs::remove_file(&installation.config_path).expect("remove configuration");
        assert!(matches!(
            installation.load(&executable),
            Err(ServiceConfigError::Config(ConfigError::Io { .. }))
        ));
    }

    #[test]
    fn a_configuration_of_another_installation_is_refused() {
        let installation = installation();
        let executable = install_worker(&installation.prefix, VERSION);
        let other = self::installation();

        assert!(matches!(
            load_service_config(
                &installation.config_path,
                installation.uid,
                &other.paths,
                &executable,
            ),
            Err(ServiceConfigError::Config(
                ConfigError::NamespaceMismatch { .. }
            ))
        ));
        assert!(matches!(
            load_service_config(
                &installation.config_path,
                installation.uid.wrapping_add(1),
                &installation.paths,
                &executable,
            ),
            Err(ServiceConfigError::Config(
                ConfigError::NamespaceMismatch { .. }
            ))
        ));
    }

    #[test]
    fn an_executable_outside_the_recorded_layout_is_refused() {
        let installation = installation();
        install_worker(&installation.prefix, VERSION);
        let stray = installation.prefix.join("pohunek-sessiond");
        fs::write(&stray, b"#!/bin/sh\n").expect("write stray worker");
        let renamed = installation
            .prefix
            .join("libexec/pohunek")
            .join(VERSION)
            .join("pohunekd");
        fs::write(&renamed, b"#!/bin/sh\n").expect("write misnamed worker");
        let elsewhere =
            pohunek_test_support::tempdir().expect("directory outside the installation");
        let foreign = install_worker(elsewhere.path(), VERSION);

        for executable in [&stray, &renamed, &foreign] {
            assert!(
                matches!(
                    installation.load(executable),
                    Err(ServiceConfigError::ForeignExecutable { .. })
                ),
                "{} must be refused",
                executable.display()
            );
        }
    }
}
