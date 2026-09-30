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
/// [`record::Store::hand_off`] and [`record::Store::adopt`]).
///
/// `SIGTERM`, `SIGINT`, and `SIGHUP` are forwarded to the child. Once it
/// exits, a holder that acquired the lock waits until every adopter, such as
/// a `pohunek service` command the child left running in the background, has
/// finished, then removes the holder record and releases the lock, so nothing
/// adopts the lock afterwards. A signal received during that wait ends the
/// wait early: the lock is released, while the adopters still running keep
/// every other transaction out through their shared lock until they end.
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
    let mut signals = Signals::new(program)?;
    let lock = match inherited::take()? {
        Some(token) => store.adopt(&token, record::Adoption::Relay)?,
        None => store.lock().await?,
    };
    let handoff = store.hand_off(&lock)?;
    let status = run_locked(handoff.token(), program, arguments, &mut signals).await;
    let (excluded, waited) = if lock.is_adopted() {
        (None, Ok(()))
    } else {
        match wait_for_adopters(&store, &mut signals).await {
            Ok(excluded) => (excluded, Ok(())),
            // Whether an adopter still runs is unknown; any that does keeps
            // its shared lock, which refuses every other transaction.
            Err(error) => (None, Err(error)),
        }
    };
    // The record goes while adopters are still excluded and before the lock,
    // so no command can adopt the lock with this token once it is free.
    let released = handoff.release();
    drop(excluded);
    drop(lock);
    let status = status?;
    waited?;
    released?;
    Ok(status)
}

/// The signals `pohunek service lock` handles while its child runs.
struct Signals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl Signals {
    fn new(program: &OsStr) -> Result<Self, Error> {
        use tokio::signal::unix::{signal, SignalKind};

        let handle = |kind| signal(kind).map_err(error::io_error("handle signals of", program));
        Ok(Self {
            terminate: handle(SignalKind::terminate())?,
            interrupt: handle(SignalKind::interrupt())?,
            hangup: handle(SignalKind::hangup())?,
        })
    }

    /// Waits for the next handled signal.
    async fn recv(&mut self) -> nix::sys::signal::Signal {
        use nix::sys::signal::Signal;

        tokio::select! {
            _ = self.terminate.recv() => Signal::SIGTERM,
            _ = self.interrupt.recv() => Signal::SIGINT,
            _ = self.hangup.recv() => Signal::SIGHUP,
        }
    }
}

/// Runs the child of [`lock`] with the holder `token`, forwarding `signals`.
///
/// The child runs in a process group of its own, so a signal meant for the
/// command reaches it exactly once: a signal sent to this process (alone or
/// to its group) is forwarded to the child's group, and when this process
/// runs in the foreground of the terminal on stdin, the terminal's
/// foreground passes to the child's group for the child's lifetime, as a
/// shell hands it to a job. Terminal signals such as `Ctrl-C` then reach the
/// child's group directly and not this process, and the child can still read
/// from the terminal.
///
/// With the terminal handed over, a stopped child (`Ctrl-Z`, or a read
/// from the terminal in the background) stops this process too, the way a
/// job stops: the foreground returns to this process's group, which then
/// stops itself so the shell that runs it sees the job stopped. When the
/// shell continues it, the foreground goes back to the child's group if
/// this process got it back (`fg`), and the child's group is continued.
async fn run_locked(
    token: &inherited::Token,
    program: &OsStr,
    arguments: &[OsString],
    signals: &mut Signals,
) -> Result<ExitStatus, Error> {
    use std::os::unix::process::CommandExt as _;

    let child = std::process::Command::new(program)
        .args(arguments)
        .env(inherited::LOCK_TOKEN_ENV, token.as_str())
        .process_group(0)
        .spawn()
        .map_err(error::io_error("run", program))?;
    let pid = i32::try_from(child.id())
        .map(nix::unistd::Pid::from_raw)
        .map_err(|_overflow| {
            error::io_error("wait for", program)(std::io::ErrorKind::InvalidData.into())
        })?;
    // The child's PID is its group ID, and stays valid until the waiter
    // below reaps it.
    let group = pid;
    let (events, mut changes) = tokio::sync::mpsc::unbounded_channel();
    let waiter = std::thread::spawn(move || watch(pid, &events));
    let terminal = Foreground::hand_to(group);
    let status = loop {
        tokio::select! {
            change = changes.recv() => match change {
                Some(Change::Stopped) => {
                    if let Some(terminal) = &terminal {
                        terminal.suspend();
                    }
                }
                Some(Change::Exited(status)) => break status,
                None => break Err(std::io::Error::other("the child waiter ended early")),
            },
            signal = signals.recv() => forward(Some(group), signal),
        }
    };
    drop(terminal);
    let _ = waiter.join();
    // `child` is reaped already; dropping it neither waits nor kills.
    drop(child);
    status.map_err(error::io_error("wait for", program))
}

/// A state change of the child `watch` reports.
#[derive(Debug)]
enum Change {
    /// The child was stopped by a signal.
    Stopped,
    /// The child ended with this status, or waiting for it failed.
    Exited(std::io::Result<ExitStatus>),
}

/// Reports `pid`'s stops and its end to `events`, reaping it.
///
/// Runs on a thread of its own: `waitpid` with `WUNTRACED` is the portable
/// way to learn that a child stopped, which the async runtime's wait does not
/// report.
fn watch(pid: nix::unistd::Pid, events: &tokio::sync::mpsc::UnboundedSender<Change>) {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use std::os::unix::process::ExitStatusExt as _;

    /// Wait-status encoding shared by Linux and macOS: the exit code in the
    /// second byte, a terminating signal in the low seven bits.
    const EXIT_CODE_SHIFT: u32 = 8;
    /// Wait-status flag of a core dump.
    const CORE_DUMPED: i32 = 0x80;

    loop {
        let change = match waitpid(pid, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Stopped(..)) => Change::Stopped,
            Ok(WaitStatus::Exited(_, code)) => {
                Change::Exited(Ok(ExitStatus::from_raw(code << EXIT_CODE_SHIFT)))
            }
            Ok(WaitStatus::Signaled(_, signal, core)) => Change::Exited(Ok(ExitStatus::from_raw(
                signal as i32 | if core { CORE_DUMPED } else { 0 },
            ))),
            Ok(_) | Err(nix::errno::Errno::EINTR) => continue,
            Err(errno) => Change::Exited(Err(errno.into())),
        };
        let exited = matches!(change, Change::Exited(_));
        if events.send(change).is_err() || exited {
            return;
        }
    }
}

/// The terminal's foreground, handed to the child's process group.
///
/// Dropping it hands the foreground back to this process's group, when the
/// child's group still has it.
#[derive(Debug)]
struct Foreground {
    /// This process's group, which had the foreground.
    own: nix::unistd::Pid,
    /// The child's group.
    child: nix::unistd::Pid,
}

impl Foreground {
    /// Hands the terminal on stdin to `child` when this process owns it.
    ///
    /// Returns `None`, changing nothing, when stdin is no terminal or this
    /// process runs in its background.
    fn hand_to(child: nix::unistd::Pid) -> Option<Self> {
        use std::os::fd::AsRawFd as _;

        let stdin = std::io::stdin();
        if !nix::unistd::isatty(stdin.as_raw_fd()).unwrap_or(false) {
            return None;
        }
        let own = nix::unistd::getpgrp();
        if nix::unistd::tcgetpgrp(&stdin).ok()? != own {
            return None;
        }
        let terminal = Self { own, child };
        terminal.give().then_some(terminal)
    }

    /// Gives the foreground to the child's group, which then continues.
    ///
    /// Only this process's group may give it away; returns whether it did.
    fn give(&self) -> bool {
        let stdin = std::io::stdin();
        if nix::unistd::tcgetpgrp(&stdin).ok() != Some(self.own)
            || nix::unistd::tcsetpgrp(&stdin, self.child).is_err()
        {
            return false;
        }
        // A child that read the terminal before it owned it was stopped by
        // `SIGTTIN`; it continues now that it may read.
        let _ = nix::sys::signal::killpg(self.child, nix::sys::signal::Signal::SIGCONT);
        true
    }

    /// Takes the foreground back from the child's group, if it has it.
    ///
    /// This process is in the terminal's background then, and changing the
    /// foreground from there raises `SIGTTOU`, which would stop it; the
    /// signal is blocked on this thread for the call, which both Linux and
    /// macOS honor for `tcsetpgrp`. The foreground is only taken while the
    /// child's group holds it, never from a shell that took it meanwhile.
    fn take_back(&self) {
        use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};

        let stdin = std::io::stdin();
        if nix::unistd::tcgetpgrp(&stdin).ok() != Some(self.child) {
            return;
        }
        let mut ttou = SigSet::empty();
        ttou.add(Signal::SIGTTOU);
        let mut previous = SigSet::empty();
        if pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&ttou), Some(&mut previous)).is_ok() {
            let _ = nix::unistd::tcsetpgrp(&stdin, self.own);
            let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous), None);
        }
    }

    /// Stops this process with its stopped child, and resumes the child with it.
    ///
    /// `SIGSTOP` cannot be caught or ignored, so this process stops even in
    /// an orphaned process group; `kill` returns once it was continued.
    fn suspend(&self) {
        use nix::sys::signal::{kill, killpg, Signal};

        self.take_back();
        let _ = kill(nix::unistd::getpid(), Signal::SIGSTOP);
        // Continued in the foreground (`fg`): the child's group gets the
        // terminal again. In the background (`bg`) it only continues.
        if !self.give() {
            let _ = killpg(self.child, Signal::SIGCONT);
        }
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        self.take_back();
    }
}

/// Waits until no command holds the adopted lock and returns its exclusion.
///
/// `None` means a signal ended the wait while an adopter still ran.
async fn wait_for_adopters(
    store: &record::Store,
    signals: &mut Signals,
) -> Result<Option<pohunek_platform::filesystem::FileLock>, Error> {
    loop {
        if let Some(excluded) = store.exclude_adopters()? {
            return Ok(Some(excluded));
        }
        tokio::select! {
            () = tokio::time::sleep(settings::LOCK_POLL) => {}
            _signal = signals.recv() => return Ok(None),
        }
    }
}

/// Sends `signal` to the child's process `group`, whose leader is not reaped yet.
fn forward(group: Option<nix::unistd::Pid>, signal: nix::sys::signal::Signal) {
    if let Some(group) = group {
        // A child that exits meanwhile is reported by the waiter next.
        let _ = nix::sys::signal::killpg(group, signal);
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
        .map(|token| {
            record::Store::new(context.paths().state_dir.clone())
                .adopt(&token, record::Adoption::Transaction)
        })
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
        // The same discovery validation install runs before its first effect.
        engine::discover_for_plan(context, &plan)?;
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
                    .adopt(&token, record::Adoption::Transaction)
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
        let mut signals = Signals::new(OsStr::new("sh")).expect("handle signals");
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
            &mut signals,
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
        let status = run_locked(
            &token,
            OsStr::new("sh"),
            &shell(probe_script(&state_dir)),
            &mut signals,
        )
        .await
        .expect("run the command");
        assert_eq!(exit_code(status), 3, "a released token adopts nothing");

        let status = run_locked(
            &token,
            OsStr::new("sh"),
            &shell("exit 42".to_owned()),
            &mut signals,
        )
        .await
        .expect("run the command");
        assert_eq!(exit_code(status), 42);
        let status = run_locked(
            &token,
            OsStr::new("sh"),
            &shell("kill -TERM $$".to_owned()),
            &mut signals,
        )
        .await
        .expect("run the command");
        assert_eq!(exit_code(status), 128 + 15, "a signal maps like a shell's");

        let error = run_locked(
            &token,
            OsStr::new("/nonexistent/command"),
            &[],
            &mut signals,
        )
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
        let mut signals = Signals::new(OsStr::new("sh")).expect("handle signals");
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
        let child = run_locked(handoff.token(), OsStr::new("sh"), &arguments, &mut signals);
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
