//! `pohunek service` against the real systemd user manager.
//!
//! Each test installs into a fresh temporary prefix with temporary XDG state,
//! data, cache, config, and runtime roots, so its installation namespace
//! differs from any real installation sharing the user manager. The daemon
//! unit goes to `$XDG_RUNTIME_DIR/systemd/user` of the real session instead of
//! the production `$XDG_CONFIG_HOME/systemd/user`: the user manager loads units
//! only from its search path, which never includes a temporary config home.
//! The unit names embed the namespace, and a drop guard uninstalls them.
//!
//! Requirements: `POHUNEK_SYSTEMD_E2E=1`, a running user manager whose
//! environment carries `DBUS_SESSION_BUS_ADDRESS` (the daemon runs with the
//! temporary `XDG_RUNTIME_DIR`, so it cannot fall back to `$XDG_RUNTIME_DIR/bus`),
//! `systemd-analyze`, and absolute `POHUNEK_DAEMON_BIN` and
//! `POHUNEK_WORKER_BIN` built from the same commit as this test.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Output};

use pohunek_cli::service::report::StatusReport;
use pohunek_cli::service::settings::LAUNCHCTL_COMMAND;
use pohunek_cli::service::{Backend, Context, Engine, Error, UninstallOptions, VERSION};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use tempfile::TempDir;

const E2E_VARIABLE: &str = "POHUNEK_SYSTEMD_E2E";

fn require_e2e() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "set {E2E_VARIABLE}=1 explicitly"
    );
}

fn required_binary(variable: &str) -> PathBuf {
    let path = std::env::var_os(variable).map_or_else(
        || panic!("{variable} must name the built binary"),
        PathBuf::from,
    );
    assert!(path.is_absolute(), "{variable} must be absolute");
    path
}

/// One isolated installation below a temporary root.
struct Installation {
    root: TempDir,
    context: Context,
    env: Vec<(&'static str, OsString)>,
}

impl Installation {
    fn new() -> Self {
        let root = pohunek_test_support::tempdir().expect("temp root");
        let dir = |name: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).expect("create XDG root");
            path
        };
        let env = vec![
            ("XDG_RUNTIME_DIR", dir("run").into_os_string()),
            ("XDG_STATE_HOME", dir("state").into_os_string()),
            ("XDG_DATA_HOME", dir("data").into_os_string()),
            ("XDG_CACHE_HOME", dir("cache").into_os_string()),
            ("XDG_CONFIG_HOME", dir("config").into_os_string()),
            ("HOME", dir("home").into_os_string()),
        ];
        let value = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, v)| v.clone())
        };
        let paths = BasePaths::resolve_for(
            Platform::current().expect("platform"),
            nix::unistd::Uid::effective().as_raw(),
            &PathEnv {
                xdg_runtime_dir: value("XDG_RUNTIME_DIR"),
                xdg_data_home: value("XDG_DATA_HOME"),
                xdg_state_home: value("XDG_STATE_HOME"),
                xdg_cache_home: value("XDG_CACHE_HOME"),
                xdg_config_home: value("XDG_CONFIG_HOME"),
                home: value("HOME"),
            },
        )
        .expect("resolve isolated paths");
        let real_runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .expect("XDG_RUNTIME_DIR is set for a user manager");
        let context = Context::new(
            paths,
            nix::unistd::Uid::effective().as_raw(),
            value("HOME").map(PathBuf::from),
            value("XDG_RUNTIME_DIR").map(PathBuf::from),
            real_runtime.join("systemd/user"),
            PathBuf::from(env!("CARGO_BIN_EXE_pohunek")),
        );
        Self { root, context, env }
    }

    fn prefix(&self) -> PathBuf {
        self.root.path().join("prefix")
    }

    /// Stages this build's three binaries like a release archive.
    fn stage(&self) -> PathBuf {
        let from = self.root.path().join("archive");
        std::fs::create_dir_all(&from).expect("archive dir");
        for (name, source) in [
            ("pohunek", PathBuf::from(env!("CARGO_BIN_EXE_pohunek"))),
            ("pohunekd", required_binary("POHUNEK_DAEMON_BIN")),
            ("pohunek-sessiond", required_binary("POHUNEK_WORKER_BIN")),
        ] {
            std::fs::copy(&source, from.join(name)).expect("stage binary");
        }
        from
    }

    /// Runs the real CLI against this installation's daemon.
    fn cli(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pohunek"));
        command.args(args).env_clear();
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.output().expect("run pohunek")
    }

    async fn status(&self, backend: &Backend) -> StatusReport {
        let config = pohunek_service_config::ServiceConfig::load(&self.context.config_path())
            .expect("load service.toml");
        Engine::new(&self.context, backend)
            .status(Some(&config))
            .await
            .expect("status")
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        // Best-effort teardown so a failed assertion never leaves a unit
        // behind in the operator's user manager.
        let Ok(runtime) = tokio::runtime::Runtime::new() else {
            return;
        };
        runtime.block_on(async {
            if let Ok(namespace) = self.context.namespace() {
                if let Ok(backend) =
                    Backend::connect(&self.context, &namespace, LAUNCHCTL_COMMAND).await
                {
                    let _ignored = Engine::new(&self.context, &backend)
                        .uninstall(UninstallOptions {
                            stop_sessions: true,
                            purge: true,
                        })
                        .await;
                    let _ignored = backend.daemon().uninstall().await;
                }
            }
        });
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"]
fn install_status_upgrade_refuse_and_uninstall_against_the_user_manager() {
    require_e2e();
    let installation = Installation::new();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let from = installation.stage();
        let namespace = installation.context.namespace().expect("namespace");
        let backend = Backend::connect(&installation.context, &namespace, LAUNCHCTL_COMMAND)
            .await
            .expect("connect to the user manager");
        let engine = Engine::new(&installation.context, &backend);

        let report = engine
            .install(&from, &installation.prefix(), VERSION)
            .await
            .expect("install");
        assert_eq!(report.namespace, namespace.as_str());
        let unit = installation
            .context
            .supervisor_dir()
            .join(namespace.daemon_unit());
        assert!(unit.is_file(), "{} was not installed", unit.display());

        let status = installation.status(&backend).await;
        assert_eq!(status.active_version.as_deref(), Some(VERSION));
        let daemon = status.daemon.expect("daemon job");
        assert_eq!(daemon.state, "running");
        assert_eq!(
            daemon.executable.as_deref(),
            Some(
                installation
                    .prefix()
                    .join("libexec/pohunek")
                    .join(VERSION)
                    .join("pohunekd")
                    .as_path()
            )
        );

        let upgrade = engine.upgrade(&from, VERSION).await.expect("upgrade");
        assert!(upgrade.unchanged, "one build can only re-install itself");

        assert_success(&installation.cli(&["session", "new", "--agent", "shell", "--json"]));
        let refused = engine.uninstall(UninstallOptions::default()).await;
        assert!(
            matches!(refused, Err(Error::LiveSessions { .. })),
            "{refused:?}"
        );
        assert!(unit.is_file(), "a refused uninstall changes nothing");

        let removed = engine
            .uninstall(UninstallOptions {
                stop_sessions: true,
                purge: false,
            })
            .await
            .expect("uninstall with --stop-sessions");
        assert_eq!(removed.stopped_sessions.len(), 1);
        assert!(!unit.exists());
        assert!(!installation.context.config_path().exists());
        assert!(backend
            .workers()
            .discover()
            .await
            .expect("discover")
            .is_empty());
        assert!(!installation
            .prefix()
            .join("libexec/pohunek")
            .join(VERSION)
            .exists());
    });
}
