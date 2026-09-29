//! Installs, upgrades, uninstalls, and reports the native pohunek service.
//!
//! This is the engine behind
//! `pohunek service install|upgrade|uninstall|status|check|lock`.
//! It installs versioned binaries under `<prefix>/libexec/pohunek/<version>/`,
//! writes the owner-private `service.toml`, and registers the daemon as a
//! systemd user unit (Linux) or a launchd login agent (macOS) through the
//! platform supervisor backends. Install and upgrade are journaled
//! transactions; see [`engine`] for the exact steps, resume, and rollback
//! rules, and [`report`] for the `--json` result shapes.
//!
//! `check` runs the checks install or upgrade make before their first effect
//! without making any, and `lock` runs another command while holding the
//! transaction lock (see [`inherited`] for how that command's own `pohunek
//! service` calls reuse it).
//!
//! The functions here resolve the host [`Context`], connect the native
//! [`Backend`], and run the [`Engine`]. Tests and alternative front ends can
//! assemble those pieces explicitly instead, for example to point the
//! systemd backend at another unit directory.

// Rust guideline compliant 2026-09-29

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use pohunek_platform::supervisor::Namespace;
use pohunek_service_config::ServiceConfig;

pub mod backend;
pub mod context;
pub mod definition;
pub mod engine;
pub mod error;
pub mod inherited;
pub mod layout;
pub mod record;
pub mod report;
pub mod settings;
pub mod usage;
#[cfg(target_os = "linux")]
pub mod verify;

#[doc(inline)]
pub use backend::Backend;
#[doc(inline)]
pub use context::Context;
#[doc(inline)]
pub use engine::{Engine, UninstallOptions};
#[doc(inline)]
pub use error::Error;

/// Version of this CLI and of the binaries it installs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Installs this CLI's version from `from` below `prefix`.
///
/// `from` defaults to the directory of the running `pohunek`, and `prefix`
/// to `$HOME/.local`.
///
/// # Errors
///
/// Returns [`Error`] for invalid paths, an existing installation, failed
/// verification, or a failed transaction step.
pub async fn install(
    from: Option<PathBuf>,
    prefix: Option<PathBuf>,
) -> Result<report::InstallReport, Error> {
    let context = Context::resolve()?;
    let inherited = inherited_lock(&context)?;
    let from = staged_dir(&context, from)?;
    let prefix = match prefix {
        Some(prefix) => absolute("--prefix", prefix)?,
        None => context.default_prefix()?,
    };
    let namespace = context.namespace()?;
    let backend = Backend::connect(&context, &namespace, settings::LAUNCHCTL_COMMAND).await?;
    engine(&context, &backend, inherited)
        .install(&from, &prefix, VERSION)
        .await
}

/// Upgrades the installation to this CLI's version from `from`.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation, and the same
/// errors as [`install`] otherwise.
pub async fn upgrade(from: Option<PathBuf>) -> Result<report::UpgradeReport, Error> {
    let context = Context::resolve()?;
    let inherited = inherited_lock(&context)?;
    let from = staged_dir(&context, from)?;
    let backend = connect_installed(&context).await?;
    engine(&context, &backend, inherited)
        .upgrade(&from, VERSION)
        .await
}

/// Uninstalls the service.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation,
/// [`Error::LiveSessions`] while sessions are live and `--stop-sessions` is
/// absent, and backend errors otherwise.
pub async fn uninstall(options: UninstallOptions) -> Result<report::UninstallReport, Error> {
    let context = Context::resolve()?;
    let inherited = inherited_lock(&context)?;
    let backend = connect_installed(&context).await?;
    engine(&context, &backend, inherited)
        .uninstall(options)
        .await
}

/// Checks whether this CLI's install or upgrade would pass its preflight.
///
/// The transaction is the one `packaging/install-daemon.sh` picks: `install`
/// while an install is pending or nothing is installed, `upgrade` otherwise.
/// `prefix` is the install prefix, `$HOME/.local` by default; for an upgrade
/// it must be omitted or equal the installed prefix. The checks run in the
/// same functions install and upgrade call before their first effect, so
/// their rules cannot drift apart. An install that would start over also
/// asks the service manager whether a daemon job is registered, last, after
/// every local check passed.
///
/// # Errors
///
/// Returns the error install or upgrade would return before its first
/// effect, [`Error::PrefixMismatch`], or [`Error::InheritedLock`] when
/// `POHUNEK_SERVICE_LOCK_TOKEN` proves no live lock holder.
pub async fn check(prefix: Option<PathBuf>) -> Result<report::CheckReport, Error> {
    let context = Context::resolve()?;
    let inherited = inherited_lock(&context)?;
    let checked = check_local(&context, prefix, VERSION, inherited.is_some())?;
    if checked.fresh_install {
        let namespace = context.namespace()?;
        let backend = Backend::connect(&context, &namespace, settings::LAUNCHCTL_COMMAND).await?;
        engine::ensure_no_daemon_job(&backend).await?;
    }
    Ok(checked.report)
}

/// Runs `program` with `arguments` while holding the transaction lock.
///
/// The lock is taken like install takes it, or adopted from a `pohunek
/// service lock` ancestor. This process keeps it; the child only receives
/// the holder token in [`inherited::LOCK_TOKEN_ENV`], with which it and the
/// `pohunek service` commands it runs adopt the lock (see
/// [`record::Store::hand_off`]). Once the child exits the holder record is
/// removed and the lock released, so a background process the child left
/// behind can neither keep nor adopt the lock. `SIGINT` and `SIGHUP` are
/// left to the child, which shares the terminal's process group, and
/// `SIGTERM` is forwarded to it.
///
/// # Errors
///
/// Returns [`Error::TransactionInProgress`] while another command holds the
/// lock, [`Error::InheritedLock`] for a token that proves no live holder,
/// [`Error::Io`] when the child cannot be spawned or waited for, and a
/// filesystem error when the holder record cannot be written or removed.
pub async fn lock(program: &OsStr, arguments: &[OsString]) -> Result<ExitStatus, Error> {
    let context = Context::resolve()?;
    let store = record::Store::new(context.paths().state_dir.clone());
    let lock = match inherited::take()? {
        Some(token) => store.adopt(&token)?,
        None => store.lock().await?,
    };
    let handoff = store.hand_off(&lock)?;
    let status = run_locked(handoff.token(), program, arguments).await;
    // The record goes before the lock, so no command can adopt the lock
    // with this token once it is free.
    let released = handoff.release();
    drop(lock);
    let status = status?;
    released?;
    Ok(status)
}

/// Runs the child of [`lock`] with the holder `token`.
async fn run_locked(
    token: &inherited::Token,
    program: &OsStr,
    arguments: &[OsString],
) -> Result<ExitStatus, Error> {
    use tokio::signal::unix::{signal, SignalKind};

    let signals = |kind| signal(kind).map_err(error::io_error("handle signals of", program));
    let mut terminate = signals(SignalKind::terminate())?;
    let mut interrupt = signals(SignalKind::interrupt())?;
    let mut hangup = signals(SignalKind::hangup())?;
    let mut child = tokio::process::Command::new(program)
        .args(arguments)
        .env(inherited::LOCK_TOKEN_ENV, token.as_str())
        .spawn()
        .map_err(error::io_error("run", program))?;
    loop {
        tokio::select! {
            status = child.wait() => return status.map_err(error::io_error("wait for", program)),
            _ = terminate.recv() => forward_terminate(child.id()),
            _ = interrupt.recv() => {}
            _ = hangup.recv() => {}
        }
    }
}

/// Sends `SIGTERM` to the child `pid`, which is not reaped yet.
fn forward_terminate(pid: Option<u32>) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        // A child that exits meanwhile is reported by `wait` next.
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

/// Exit status of `pohunek service lock` for its child's `status`.
///
/// A child killed by a signal maps to 128 plus the signal number, the
/// convention of POSIX shells, so a caller sees the same status whether or
/// not the command ran under the lock.
#[must_use]
pub fn exit_code(status: ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt as _;

    /// Offset POSIX shells add to a terminating signal's number.
    const SIGNAL_EXIT_BASE: i32 = 128;
    /// Status reported for a child that neither exited nor was killed.
    const UNKNOWN_EXIT: u8 = 1;

    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT_BASE + signal))
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(UNKNOWN_EXIT)
}

/// Adopts the transaction lock of a `pohunek service lock` ancestor.
///
/// # Errors
///
/// Returns [`Error::InheritedLock`] when `POHUNEK_SERVICE_LOCK_TOKEN` was set
/// but proves no live holder of `context`'s transaction lock.
fn inherited_lock(context: &Context) -> Result<Option<record::TransactionLock>, Error> {
    inherited::take()?
        .map(|token| record::Store::new(context.paths().state_dir.clone()).adopt(&token))
        .transpose()
}

fn engine<'a>(
    context: &'a Context,
    backend: &'a Backend,
    inherited: Option<record::TransactionLock>,
) -> Engine<'a> {
    let engine = Engine::new(context, backend);
    match inherited {
        Some(lock) => engine.with_inherited_lock(lock),
        None => engine,
    }
}

/// The local part of [`check`], and whether its daemon-job check applies.
#[derive(Debug)]
pub(crate) struct Checked {
    /// The report a passing check returns.
    pub(crate) report: report::CheckReport,
    /// Whether install would start over and must find no daemon job
    /// ([`engine::ensure_no_daemon_job`]).
    pub(crate) fresh_install: bool,
}

/// Runs the checks of [`check`] that need no service manager.
///
/// The checks are the ones install or upgrade make before any effect, in
/// the same functions: [`engine::install_preflight`] or
/// [`engine::upgrade_preflight`] with [`engine::check_layout_dirs`], the
/// pending-transaction decision ([`engine::install_plan`] or
/// [`engine::upgrade_plan`]), the recorded installation's verification, and
/// the prefix claim ([`layout::verify_claim`]). Nothing is written except the
/// owner-private application state and runtime roots, which every service
/// command prepares to derive the namespace.
///
/// # Errors
///
/// See [`check`].
pub(crate) fn check_local(
    context: &Context,
    prefix: Option<PathBuf>,
    version: &str,
    locked: bool,
) -> Result<Checked, Error> {
    let prefix = prefix
        .map(|prefix| absolute("--prefix", prefix))
        .transpose()?;
    let config_path = context.config_path();
    let pending = record::Store::new(context.paths().state_dir.clone()).load()?;
    let pending_install = pending
        .as_ref()
        .is_some_and(|record| record.operation == record::Operation::Install);
    let (operation, prefix, namespace, plan) = if pending_install || !exists(&config_path)? {
        let prefix = match prefix {
            Some(prefix) => prefix,
            None => context.default_prefix()?,
        };
        let layout = engine::install_preflight(context, &prefix)?;
        let plan = engine::install_plan(pending, &prefix, version)?;
        // A rollback of the pending record removes the `service.toml` it wrote.
        if matches!(plan, engine::Plan::Fresh) && exists(&config_path)? {
            return Err(Error::AlreadyInstalled { path: config_path });
        }
        let namespace = context.namespace()?;
        layout::verify_claim(&layout, &namespace)?;
        (record::Operation::Install, prefix, namespace, plan)
    } else {
        let config = verified_config(context)?;
        engine::upgrade_preflight(context)?;
        let plan = engine::upgrade_plan(pending, version)?;
        let layout = config.layout();
        if let Some(requested) = prefix.filter(|requested| requested != layout.prefix()) {
            return Err(Error::PrefixMismatch {
                requested,
                installed: layout.prefix().to_path_buf(),
            });
        }
        engine::check_layout_dirs(context, layout)?;
        layout::verify_claim(layout, &config.namespace())?;
        (
            record::Operation::Upgrade,
            layout.prefix().to_path_buf(),
            config.namespace(),
            plan,
        )
    };
    let fresh_install =
        operation == record::Operation::Install && !matches!(plan, engine::Plan::Resume(_));
    let report = report::CheckReport {
        operation: operation.as_str(),
        version: version.to_owned(),
        prefix,
        namespace: namespace.as_str().to_owned(),
        config_path,
        pending_transaction: plan.record().map(engine::pending_report),
        pending_action: match plan {
            engine::Plan::Fresh => None,
            engine::Plan::Resume(_) => Some("resume"),
            engine::Plan::RollBack(_) => Some("roll_back"),
        },
        locked,
    };
    Ok(Checked {
        report,
        fresh_install,
    })
}

/// Runs [`check`] for `context` and `version` against `backend`.
///
/// # Errors
///
/// See [`check`].
#[cfg(test)]
pub(crate) async fn check_with(
    context: &Context,
    backend: &Backend,
    prefix: Option<PathBuf>,
    version: &str,
    locked: bool,
) -> Result<report::CheckReport, Error> {
    let checked = check_local(context, prefix, version, locked)?;
    if checked.fresh_install {
        engine::ensure_no_daemon_job(backend).await?;
    }
    Ok(checked.report)
}

/// Reports the installation.
///
/// Without `service.toml` the report says so and no service manager is
/// contacted.
///
/// # Errors
///
/// Returns [`Error`] for an unreadable `service.toml` or unsafe directories,
/// and [`Error::Config`] when the recorded installation does not describe
/// this process (see [`status_of`]).
pub async fn status() -> Result<report::StatusReport, Error> {
    status_of(&Context::resolve()?).await
}

/// Reports the installation `context` locates.
///
/// A recorded installation is verified like [`upgrade`] and [`uninstall`]
/// verify it: its user and canonical roots must be this process's. Otherwise
/// the report would mix the recorded namespace and prefix with the socket and
/// transaction store the current `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`
/// select, and a caller acting on it would address another installation.
///
/// # Errors
///
/// Returns [`Error::Config`] with the differing key when the verification
/// fails, before any service manager is contacted.
async fn status_of(context: &Context) -> Result<report::StatusReport, Error> {
    let config_path = context.config_path();
    if !exists(&config_path)? {
        let store = record::Store::new(context.paths().state_dir.clone());
        return Ok(report::StatusReport {
            installed: false,
            config_path,
            namespace: None,
            prefix: None,
            active_version: None,
            daemon: None,
            daemon_error: None,
            versions: Vec::new(),
            workers: Vec::new(),
            workers_error: None,
            unreadable_journals: Vec::new(),
            transaction_in_progress: store.in_progress()?,
            pending_transaction: store.load()?.map(|pending| report::PendingReport {
                operation: pending.operation.as_str(),
                version: pending.version,
                step: pending.step.as_str(),
            }),
        });
    }
    let config = verified_config(context)?;
    let backend = Backend::connect(
        context,
        &config.namespace(),
        config.deadlines().launchctl_command,
    )
    .await?;
    Engine::new(context, &backend).status(Some(&config)).await
}

fn staged_dir(context: &Context, from: Option<PathBuf>) -> Result<PathBuf, Error> {
    match from {
        Some(from) => absolute("--from", from),
        None => Ok(context
            .cli_executable()
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)),
    }
}

/// Accepts only the one normalized spelling of an absolute path.
///
/// `service.toml` rejects any other spelling of the prefix, so the flag is
/// refused here rather than silently normalized.
fn absolute(flag: &'static str, path: PathBuf) -> Result<PathBuf, Error> {
    if pohunek_paths::is_normalized_absolute(&path) {
        Ok(path)
    } else {
        Err(Error::InvalidPath { flag, path })
    }
}

/// Connects the backend of the recorded installation, or of an interrupted
/// install that has not registered anything yet.
///
/// With `service.toml` the backend is built from the verified installation:
/// [`verified_connection`] confirms the recorded user and canonical roots
/// describe this process, and the namespace and `launchctl` deadline come
/// from the recording, never from the current environment alone. A changed
/// `XDG_STATE_HOME` or `XDG_RUNTIME_DIR` would otherwise point the backend at
/// another installation's jobs while `service.toml` still names the original
/// one. Without `service.toml` only a pre-registration install connects,
/// against the namespace this process derives: nothing of that transaction
/// can have been registered yet.
async fn connect_installed(context: &Context) -> Result<Backend, Error> {
    let config_path = context.config_path();
    if exists(&config_path)? {
        let (namespace, deadline) = verified_connection(context)?;
        return Backend::connect(context, &namespace, deadline).await;
    }
    let pending = record::Store::new(context.paths().state_dir.clone()).load()?;
    if pending.is_none() {
        return Err(Error::NotInstalled { path: config_path });
    }
    Backend::connect(context, &context.namespace()?, settings::LAUNCHCTL_COMMAND).await
}

/// Verifies the installation recorded at `context.config_path()` against the
/// running process and returns the namespace and `launchctl` deadline its
/// backend must use.
///
/// The caller has checked that the file exists.
///
/// # Errors
///
/// Returns [`Error::Config`] when the file cannot be read or its recorded
/// namespace inputs differ from this process's user or canonical roots.
fn verified_connection(context: &Context) -> Result<(Namespace, Duration), Error> {
    let config = verified_config(context)?;
    Ok((config.namespace(), config.deadlines().launchctl_command))
}

/// Loads the installation recorded at `context.config_path()` and verifies
/// its namespace inputs against the running process.
///
/// # Errors
///
/// See [`verified_connection`].
fn verified_config(context: &Context) -> Result<ServiceConfig, Error> {
    let config = ServiceConfig::load(&context.config_path())?;
    config.verify_installation(
        context.uid(),
        &context.paths().state_dir,
        &context.paths().runtime_dir,
    )?;
    Ok(config)
}

fn exists(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_metadata) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error::io_error("inspect", path)(error)),
    }
}

#[cfg(test)]
mod tests {
    use pohunek_paths::{BasePaths, PathEnv, Platform};
    use pohunek_service_config::ConfigError;

    use super::*;
    use crate::service::context::tests::temp_root;
    use crate::service::definition::initial_config;

    /// Resolves paths from an XDG layout whose state and runtime roots a test
    /// can move independently; the config home pins where `service.toml` is
    /// read from.
    fn moved_paths(
        root: &Path,
        config_home: &Path,
        state_home: &Path,
        runtime_dir: &Path,
    ) -> BasePaths {
        let env = PathEnv {
            xdg_runtime_dir: Some(OsString::from(runtime_dir)),
            xdg_data_home: Some(OsString::from(root.join("data"))),
            xdg_state_home: Some(OsString::from(state_home)),
            xdg_cache_home: Some(OsString::from(root.join("cache"))),
            xdg_config_home: Some(OsString::from(config_home)),
            home: Some(OsString::from(root.join("home"))),
        };
        BasePaths::resolve_for(
            Platform::current().expect("supported platform"),
            nix::unistd::Uid::effective().as_raw(),
            &env,
        )
        .expect("resolve test paths")
    }

    fn context_of(
        root: &Path,
        config_home: &Path,
        state_home: &Path,
        runtime_dir: &Path,
        uid: u32,
    ) -> Context {
        Context::new(
            moved_paths(root, config_home, state_home, runtime_dir),
            uid,
            Some(root.join("home")),
            Some(runtime_dir.to_path_buf()),
            root.join("units"),
            PathBuf::from("/usr/bin/pohunek"),
        )
    }

    /// Writes the `service.toml` an install of `context` would record.
    fn install_config(context: &Context) {
        let config = initial_config(context, &context.default_prefix().expect("home"), "1.0.0")
            .expect("valid config");
        config.write(&context.config_path()).expect("write config");
    }

    /// The installed context and the canonical roots it recorded.
    fn installed() -> (tempfile::TempDir, PathBuf, Context, PathBuf) {
        let (temp, root) = temp_root();
        let context = context_of(
            &root,
            &root.join("config"),
            &root.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        install_config(&context);
        let state_root = context.roots().expect("roots").0;
        (temp, root, context, state_root)
    }

    #[test]
    fn verified_connection_accepts_the_environment_the_installation_records() {
        let (_temp, _root, context, _state) = installed();
        let (namespace, deadline) = verified_connection(&context).expect("verified");
        assert_eq!(namespace, context.namespace().expect("namespace"));
        assert_eq!(
            deadline,
            ServiceConfig::load(&context.config_path())
                .expect("config")
                .deadlines()
                .launchctl_command
        );
    }

    #[test]
    fn verified_connection_fails_closed_when_xdg_state_home_changed() {
        let (_temp, root, _context, state_root) = installed();
        let (moved_temp, moved) = temp_root();

        // Only XDG_STATE_HOME moves; the config home still locates
        // `service.toml`.
        let changed = context_of(
            &root,
            &root.join("config"),
            &moved.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        changed.roots().expect("the moved roots resolve");
        let error = verified_connection(&changed).expect_err("mismatch");
        let Error::Config(ConfigError::NamespaceMismatch {
            key,
            recorded: old,
            actual,
        }) = &error
        else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(*key, "namespace.state_root");
        assert_eq!(old, &state_root.display().to_string());
        assert_eq!(actual, &moved.join("state/pohunek").display().to_string());
        drop(moved_temp);
        assert_eq!(error.code(), "service_config_invalid");
    }

    #[test]
    fn verified_connection_fails_closed_when_xdg_runtime_dir_changed() {
        let (_temp, root, _context, _state) = installed();
        let (moved_temp, moved) = temp_root();

        // Only XDG_RUNTIME_DIR moves; the daemon socket the backend would
        // address lives under the moved runtime root, not the recorded one.
        let changed = context_of(
            &root,
            &root.join("config"),
            &root.join("state"),
            &moved.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        changed.roots().expect("the moved roots resolve");
        let error = verified_connection(&changed).expect_err("mismatch");
        assert!(
            matches!(
                &error,
                Error::Config(ConfigError::NamespaceMismatch { key, .. }) if *key == "namespace.runtime_root"
            ),
            "{error:?}"
        );
        drop(moved_temp);
    }

    #[test]
    fn verified_connection_fails_closed_for_another_user() {
        let (_temp, root, _context, _state) = installed();
        let foreign = context_of(
            &root,
            &root.join("config"),
            &root.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw() + 1,
        );
        let error = verified_connection(&foreign).expect_err("mismatch");
        assert!(
            matches!(
                &error,
                Error::Config(ConfigError::NamespaceMismatch { key, .. }) if *key == "namespace.uid"
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn status_refuses_a_report_when_the_roots_changed() {
        let (_temp, root, _context, _state) = installed();
        for (state_home, runtime_dir, key) in [
            ("moved-state", "run", "namespace.state_root"),
            ("state", "moved-run", "namespace.runtime_root"),
        ] {
            let changed = context_of(
                &root,
                &root.join("config"),
                &root.join(state_home),
                &root.join(runtime_dir),
                nix::unistd::Uid::effective().as_raw(),
            );
            changed.roots().expect("the moved roots resolve");

            // The mismatch fails before any service manager is contacted, so
            // the assertion holds with and without a session bus.
            let error = status_of(&changed).await.expect_err("refused");
            assert!(
                matches!(
                    &error,
                    Error::Config(ConfigError::NamespaceMismatch { key: rejected, .. }) if *rejected == key
                ),
                "{error:?}"
            );
            assert_eq!(error.code(), "service_config_invalid");
        }
    }

    /// Names the state directory `inherited_lock_probe` adopts the lock in;
    /// unset in an ordinary test run, where the probe passes without checking.
    const PROBE_STATE_ENV: &str = "POHUNEK_TEST_PROBE_STATE_DIR";

    /// Probe exit status: the token adopted the lock.
    const PROBE_ADOPTED: u8 = 7;

    /// Probe exit status: the token did not adopt the lock.
    const PROBE_REFUSED: i32 = 3;

    /// Child side of `lock_runs_the_command_with_a_token_that_adopts_the_lock`.
    ///
    /// Adopts the lock in the state directory the parent names with the token
    /// [`inherited::LOCK_TOKEN_ENV`] carries, exactly as a `pohunek service`
    /// command under the lock does.
    #[test]
    fn inherited_lock_probe() {
        let Some(state_dir) = std::env::var_os(PROBE_STATE_ENV) else {
            return;
        };
        let adopted = std::env::var(inherited::LOCK_TOKEN_ENV)
            .ok()
            .and_then(|token| inherited::Token::parse(&token).ok())
            .is_some_and(|token| {
                record::Store::new(PathBuf::from(state_dir))
                    .adopt(&token)
                    .is_ok()
            });
        std::process::exit(if adopted {
            i32::from(PROBE_ADOPTED)
        } else {
            PROBE_REFUSED
        });
    }

    fn shell(script: String) -> [OsString; 2] {
        [OsString::from("-c"), OsString::from(script)]
    }

    /// A shell script that runs `inherited_lock_probe` against `state_dir`.
    fn probe_script(state_dir: &Path) -> String {
        let probe = std::env::current_exe().expect("test binary");
        format!(
            "{PROBE_STATE_ENV}='{}' exec '{}' --exact service::tests::inherited_lock_probe --quiet",
            state_dir.display(),
            probe.display()
        )
    }

    #[tokio::test]
    async fn lock_runs_the_command_with_a_token_that_adopts_the_lock() {
        let (_temp, root) = temp_root();
        let state_dir = root.join("state/pohunek");
        let store = record::Store::new(state_dir.clone());
        let lock = store.lock().await.expect("take the lock");
        let handoff = store.hand_off(&lock).expect("publish the holder record");

        // The child receives only the token, and adopts the lock with it.
        let status = run_locked(
            handoff.token(),
            OsStr::new("sh"),
            &shell(probe_script(&state_dir)),
        )
        .await
        .expect("run the command");
        assert_eq!(
            exit_code(status),
            PROBE_ADOPTED,
            "the child adopts the lock"
        );

        // Once the record is gone, the same token adopts nothing.
        let token = handoff.token().clone();
        handoff.release().expect("remove the holder record");
        let status = run_locked(&token, OsStr::new("sh"), &shell(probe_script(&state_dir)))
            .await
            .expect("run the command");
        assert_eq!(exit_code(status), 3, "a released token adopts nothing");

        let status = run_locked(&token, OsStr::new("sh"), &shell("exit 42".to_owned()))
            .await
            .expect("run the command");
        assert_eq!(exit_code(status), 42);
        let status = run_locked(&token, OsStr::new("sh"), &shell("kill -TERM $$".to_owned()))
            .await
            .expect("run the command");
        assert_eq!(exit_code(status), 128 + 15, "a signal maps like a shell's");

        let error = run_locked(&token, OsStr::new("/nonexistent/command"), &[])
            .await
            .expect_err("a missing command");
        assert!(
            matches!(
                &error,
                Error::Io {
                    operation: "run",
                    ..
                }
            ),
            "{error:?}"
        );
        drop(lock);
    }

    #[tokio::test]
    async fn no_other_transaction_starts_while_the_locked_command_runs() {
        let (_temp, root) = temp_root();
        let store = record::Store::new(root.join("state/pohunek"));
        let lock = store.lock().await.expect("take the lock");
        let handoff = store.hand_off(&lock).expect("publish the holder record");
        let stop = root.join("stop");
        let script = format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done; exit 3",
            stop.display()
        );
        let arguments = shell(script);
        let child = run_locked(handoff.token(), OsStr::new("sh"), &arguments);
        let competitor = async {
            let refused = store.lock().await;
            std::fs::write(&stop, "").expect("release the child");
            refused
        };
        let (status, refused) = tokio::join!(child, competitor);
        assert_eq!(exit_code(status.expect("run the command")), 3);
        assert!(
            matches!(refused, Err(Error::TransactionInProgress { .. })),
            "{refused:?}"
        );
        handoff.release().expect("remove the holder record");
        drop(lock);
        store
            .lock()
            .await
            .expect("the lock is free once the command ended");
    }

    #[tokio::test]
    async fn connect_installed_refuses_to_connect_when_the_roots_changed() {
        let (_temp, root, _context, _state) = installed();
        let (moved_temp, moved) = temp_root();
        let changed = context_of(
            &root,
            &root.join("config"),
            &moved.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        changed.roots().expect("the moved roots resolve");

        // The mismatch fails before any service manager is contacted, so the
        // assertion holds with and without a session bus.
        let error = connect_installed(&changed).await.expect_err("refused");
        assert!(
            matches!(&error, Error::Config(ConfigError::NamespaceMismatch { .. })),
            "{error:?}"
        );
        drop(moved_temp);
    }
}
