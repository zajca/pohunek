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

// Rust guideline compliant 2026-10-14

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use pohunek_platform::supervisor::Namespace;
use pohunek_service_config::{ServiceConfig, UpgradeSource};

pub mod backend;
pub mod context;
pub mod definition;
pub mod engine;
pub mod error;
pub mod inherited;
pub mod layout;
pub mod ledger;
mod legacy_store;
pub mod manager_env;
pub mod preflight;
pub mod record;
pub mod report;
mod rollback;
pub mod runtimes;
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
    install_in(&Context::resolve()?, from, prefix).await
}

/// Installs this CLI's version as [`install`] does, for the installation
/// `context` describes instead of the one the process environment selects.
///
/// # Errors
///
/// Returns the errors of [`install`].
pub async fn install_in(
    context: &Context,
    from: Option<PathBuf>,
    prefix: Option<PathBuf>,
) -> Result<report::InstallReport, Error> {
    let inherited = inherited_lock(context)?;
    let from = staged_dir(context, from)?;
    let prefix = match prefix {
        Some(prefix) => absolute("--prefix", prefix)?,
        None => context.default_prefix()?,
    };
    let namespace = context.namespace()?;
    let backend = Backend::connect(context, &namespace, settings::LAUNCHCTL_COMMAND).await?;
    engine(context, &backend, inherited)
        .install(&from, &prefix, VERSION)
        .await
}

/// Upgrades the installation to this CLI's version from `from`.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation,
/// [`Error::UpgradeAtRisk`] when live sessions would lose their runtime or
/// native recovery and `accept_runtime_loss` is `false`,
/// [`Error::UpgradeStoreUnusable`] when the new daemon cannot start with the
/// metadata store, and the same errors as [`install`] otherwise.
pub async fn upgrade(
    from: Option<PathBuf>,
    accept_runtime_loss: bool,
    preflight_budget: Duration,
) -> Result<report::UpgradeReport, Error> {
    let context = Context::resolve()?.with_preflight_budget(preflight_budget);
    let inherited = inherited_lock(&context)?;
    let from = staged_dir(&context, from)?;
    let backend = connect_installed(&context).await?;
    engine(&context, &backend, inherited)
        .with_runtime_loss_accepted(accept_runtime_loss)
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
    // Checked before the service manager is contacted: the refusal must not
    // depend on the manager answering. The unit lifecycle lock stays held until
    // the uninstall ends, so no workflow enable records units meanwhile.
    let _units = crate::workflow::units::check_no_units(&context)?;
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
pub async fn check(
    prefix: Option<PathBuf>,
    accept_runtime_loss: bool,
    preflight_budget: Duration,
) -> Result<report::CheckReport, Error> {
    let context = Context::resolve()?.with_preflight_budget(preflight_budget);
    let inherited = inherited_lock(&context)?;
    let checked = check_local(&context, prefix, VERSION, inherited.is_some())?;
    // Install's order: the rollback of an interrupted install discovers before
    // it looks at the job, a fresh install after.
    if checked.needs_discovery && checked.discovery_first {
        context.install_search_path()?;
    }
    if checked.fresh_install {
        let namespace = context.namespace()?;
        let backend = Backend::connect(&context, &namespace, settings::LAUNCHCTL_COMMAND).await?;
        engine::ensure_no_daemon_job(&backend).await?;
    }
    if checked.needs_discovery && !checked.discovery_first {
        context.install_search_path()?;
    }
    let daemon_dir = staged_dir(&context, None)?;
    let rollback_backend = if checked.rollback.is_some() {
        let namespace = context.namespace()?;
        Some(Backend::connect(&context, &namespace, settings::LAUNCHCTL_COMMAND).await?)
    } else {
        None
    };
    finish_check(
        checked,
        &preflight::DaemonPreflight,
        &context,
        rollback_backend.as_ref(),
        &daemon_dir,
        VERSION,
        accept_runtime_loss,
    )
    .await
}

/// Completes a check with the adoption preflight an upgrade that swaps the
/// daemon makes, last, after every other check passed.
///
/// # Errors
///
/// Returns the errors of [`preflight::gate`].
async fn finish_check(
    mut checked: Checked,
    preflight: &dyn preflight::AdoptionPreflight,
    context: &Context,
    backend: Option<&Backend>,
    daemon_dir: &Path,
    version: &str,
    accept_runtime_loss: bool,
) -> Result<report::CheckReport, Error> {
    // The source is classified for every adoption mode, as install and upgrade
    // classify it, so a check refuses what they would refuse.
    let release =
        layout::operator_release(&engine::install_layout(&checked.report.prefix)?, daemon_dir)?;
    if let Some(record) = &checked.rollback {
        let layout = engine::install_layout(&record.prefix)?;
        let previous = record
            .previous_version
            .as_deref()
            .ok_or_else(|| Error::Record {
                path: context.paths().state_dir.join(record::FILE_NAME),
                detail: "an upgrade record names no previous version".to_owned(),
            })?;
        let daemon_dir =
            layout
                .version_dir(previous)
                .ok_or_else(|| Error::RollbackPreflightFailed {
                    version: previous.to_owned(),
                    detail: "previous version has no valid installation directory".to_owned(),
                })?;
        let daemon_dir = layout::verify_installed(&layout, &daemon_dir)?;
        let job = engine::journal_job_path(context, record, engine::rollback_stops_daemon(record))
            .await?;
        let warnings = engine::job_warnings(context, &job);
        let mut gated = rollback::gate(
            rollback::Inputs {
                preflight,
                context,
                backend,
                layout: &layout,
                inspector: &pohunek_platform::process::HostInspector::new(),
            },
            &daemon_dir,
            previous,
            previous,
            &job.value,
            &job.variables,
            accept_runtime_loss,
        )
        .await
        .map_err(|error| error.with_warnings(&warnings))?;
        gated.warnings.clone_from(&warnings);
        checked.report.accepted_runtime_loss = gated.accepted;
        checked.report.preflight = Some(gated.report.clone());
        if let Some(pending) = &mut checked.report.pending_transaction {
            pending.accepted_runtime_loss = gated.accepted;
            pending.warnings.clone_from(&gated.warnings);
            pending.preflight = Some(gated.report);
        }
    }
    if checked.adoption == Adoption::Required {
        let job = engine::installed_job_path(context).await?;
        let warnings = engine::job_warnings(context, &job);
        // Reading the ledger changes nothing, so a check with the flag leaves the
        // state exactly as it found it.
        let acceptance = match &checked.installed_version {
            Some(installed) => {
                let store = record::Store::new(context.paths().state_dir.clone());
                ledger::Acceptance::bind(
                    store.read_ledger()?.as_deref(),
                    &store.ledger_path(),
                    context.namespace()?.as_str(),
                    installed,
                )?
            }
            None => ledger::Acceptance::none(),
        };
        let gated = preflight::gate_upgrade(
            preflight,
            context,
            &release,
            version,
            &job.value,
            &job.variables,
            preflight::ReleasePlan::Included,
            accept_runtime_loss,
            &acceptance,
        )
        .await
        .map_err(|error| error.with_warnings(&warnings))?;
        checked.report.accepted_runtime_loss |= gated.accepted;
        checked.report.acceptance = gated.acceptance;
        checked.report.preflight = Some(gated.report);
        checked.report.warnings.extend(warnings);
        checked.report.warnings.extend(gated.warnings);
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
/// shell hands it to a job: the child takes it before it executes the
/// command, and this process makes the same handoff after the spawn, so the
/// command owns the terminal from its first instruction. Terminal signals
/// such as `Ctrl-C` then reach the child's group directly and not this
/// process, and the child can read from the terminal at once.
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

    let terminal = Terminal::observe();
    let mut command = std::process::Command::new(program);
    command
        .args(arguments)
        .env(inherited::LOCK_TOKEN_ENV, token.as_str())
        .process_group(0);
    if terminal.was_foreground() {
        terminal.prepare(&mut command);
    }
    let child = command.spawn().map_err(|source| Error::CommandSpawn {
        path: program.into(),
        source,
        recovery: terminal.recover(),
    })?;
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
    #[cfg(feature = "test-util")]
    hold_before_handoff();
    let terminal = terminal
        .was_foreground()
        .then_some(terminal)
        .and_then(|terminal| terminal.hand_to(group));
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

/// Environment variable naming a FIFO that [`run_locked`] reads to its end
/// after it spawned the child and before its own handoff of the terminal.
///
/// A test holds the lock process there to run the child first, the order a
/// loaded host can produce.
#[cfg(feature = "test-util")]
pub const HANDOFF_HOLD_ENV: &str = "POHUNEK_TEST_SERVICE_LOCK_HANDOFF_HOLD";

/// Blocks until the test releases the FIFO that [`HANDOFF_HOLD_ENV`] names.
///
/// Opening the FIFO waits for the test's writer, and the read ends when the
/// test closes it.
#[cfg(feature = "test-util")]
fn hold_before_handoff() {
    if let Some(path) = std::env::var_os(HANDOFF_HOLD_ENV) {
        let _ = std::fs::read(path);
    }
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

/// The initial terminal ownership, including failed ownership observations.
#[derive(Debug, Clone, Copy)]
struct Terminal {
    own: nix::unistd::Pid,
    tty: nix::Result<bool>,
    foreground: Option<nix::Result<nix::unistd::Pid>>,
}

impl Terminal {
    /// Observes ownership without discarding a failed terminal query.
    fn observe() -> Self {
        use std::os::fd::AsRawFd as _;

        let stdin = std::io::stdin();
        let tty = nix::unistd::isatty(stdin.as_raw_fd());
        let own = nix::unistd::getpgrp();
        let foreground = (tty == Ok(true)).then(|| nix::unistd::tcgetpgrp(&stdin));
        Self {
            own,
            tty,
            foreground,
        }
    }

    fn was_foreground(self) -> bool {
        self.tty == Ok(true) && self.foreground == Some(Ok(self.own))
    }

    /// Makes the child of `command` take the terminal for its own group
    /// before it executes, as the child of a job-control shell does.
    ///
    /// A command that reads the terminal at once is then never stopped by
    /// `SIGTTIN` before the handoff, a stop the shell that runs this process
    /// would see as the job's. The child takes the terminal only while this
    /// process's group still owns it.
    fn prepare(self, command: &mut std::process::Command) {
        use nix::sys::signal::{SigSet, Signal};
        use std::os::unix::process::CommandExt as _;

        let own = self.own;
        let mut ttou = SigSet::empty();
        ttou.add(Signal::SIGTTOU);
        #[expect(
            unsafe_code,
            reason = "pre_exec is required to take the terminal between fork and exec"
        )]
        // SAFETY: the closure makes only async-signal-safe calls on values
        // built before the fork.
        unsafe {
            command.pre_exec(move || {
                take_terminal(own, ttou);
                Ok(())
            });
        }
    }

    /// Hands the terminal to the child's `group`, which normally took it
    /// before it executed.
    ///
    /// This process takes the same step in case the child did not, as the
    /// parent side of a job-control shell does. Returns `None` when neither
    /// did, because another group took the terminal meanwhile.
    fn hand_to(self, group: nix::unistd::Pid) -> Option<Foreground> {
        let terminal = Foreground {
            own: self.own,
            child: group,
        };
        let current = nix::unistd::tcgetpgrp(std::io::stdin()).ok()?;
        (current == group || terminal.give()).then_some(terminal)
    }

    /// Takes the terminal back after a failed spawn.
    ///
    /// The child may have taken it before its `exec` failed, and the spawn
    /// reaps that child, so its group has no member left. The terminal is
    /// taken back only from a group without members, never from a live one
    /// that took it meanwhile.
    fn recover(self) -> String {
        let stdin = std::io::stdin();
        let before = recovery_call("tcgetpgrp", || nix::unistd::tcgetpgrp(&stdin));
        let mut probe = None;
        let mut reclaim = None;
        if self.was_foreground() {
            if let Ok(current) = before {
                if current != self.own {
                    let result =
                        recovery_call("killpg", || nix::sys::signal::killpg(current, None));
                    probe = Some(result.map_err(raw_errno));
                    if result == Err(nix::errno::Errno::ESRCH) {
                        reclaim = Some(claim_from_background(&stdin, self.own));
                    }
                }
            }
        }
        // Observation only: no retry, extra handoff, or claim from a live group.
        let after = nix::unistd::tcgetpgrp(&stdin);
        format!(
            "owner_pgid={} initial_tty={:?} initial_foreground={:?} \
             foreground_before={:?} killpg={probe:?} reclaim=[{}] foreground_after={:?}",
            self.own,
            self.tty.map_err(raw_errno),
            self.foreground
                .map(|result| result.map(nix::unistd::Pid::as_raw).map_err(raw_errno)),
            before.map(nix::unistd::Pid::as_raw).map_err(raw_errno),
            reclaim.map_or_else(|| "not attempted".to_owned(), |report| report.to_string()),
            after.map(nix::unistd::Pid::as_raw).map_err(raw_errno),
        )
    }
}

/// Takes the terminal on stdin for this process's group, in the child
/// between `fork` and `exec`, while `owner`'s group owns it.
///
/// Only async-signal-safe calls run here. The group is created again, which
/// changes nothing when the standard library created it already. `SIGTTOU`
/// is blocked for `tcsetpgrp`, which this process calls from the terminal's
/// background, and the previous mask is restored because the command
/// inherits it.
#[expect(
    unsafe_code,
    reason = "stdin is borrowed by its descriptor number inside pre_exec"
)]
fn take_terminal(owner: nix::unistd::Pid, ttou: nix::sys::signal::SigSet) {
    use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow};
    use nix::unistd::{getpgrp, setpgid, tcgetpgrp, tcsetpgrp, Pid};

    // SAFETY: descriptor 0 stays open for the whole call; the standard
    // library sets up stdio before pre_exec closures run.
    let stdin = unsafe { std::os::fd::BorrowedFd::borrow_raw(libc::STDIN_FILENO) };
    if tcgetpgrp(stdin) != Ok(owner) || setpgid(Pid::from_raw(0), Pid::from_raw(0)).is_err() {
        return;
    }
    let mut previous = SigSet::empty();
    if pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&ttou), Some(&mut previous)).is_ok() {
        let _ = tcsetpgrp(stdin, getpgrp());
        let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous), None);
    }
}

/// Gives the terminal on `stdin` to `group` from this process, which runs in
/// the terminal's background.
///
/// Changing the foreground from there raises `SIGTTOU`, which would stop
/// this process; the signal is blocked on this thread for the call, which
/// both Linux and macOS honor for `tcsetpgrp`.
fn claim_from_background(stdin: &std::io::Stdin, group: nix::unistd::Pid) -> TerminalClaim {
    use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};

    let mut ttou = SigSet::empty();
    ttou.add(Signal::SIGTTOU);
    let mut previous = SigSet::empty();
    let mask_block = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&ttou), Some(&mut previous));
    let (handoff, mask_restore) = if mask_block.is_ok() {
        let handoff = recovery_call("tcsetpgrp", || nix::unistd::tcsetpgrp(stdin, group));
        let restore = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous), None);
        (
            Some(handoff.map_err(raw_errno)),
            Some(restore.map_err(raw_errno)),
        )
    } else {
        (None, None)
    };
    TerminalClaim {
        mask_block: mask_block.map_err(raw_errno),
        handoff,
        mask_restore,
    }
}

/// Each result of the existing handback, retained for failed-spawn diagnostics.
#[derive(Debug)]
struct TerminalClaim {
    mask_block: Result<(), i32>,
    handoff: Option<Result<(), i32>>,
    mask_restore: Option<Result<(), i32>>,
}

impl std::fmt::Display for TerminalClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mask_block={:?} tcsetpgrp={:?} mask_restore={:?}",
            self.mask_block, self.handoff, self.mask_restore
        )
    }
}

fn raw_errno(error: nix::errno::Errno) -> i32 {
    error as i32
}

/// A failed recovery syscall, injected only into the real CLI's test build.
#[cfg(feature = "test-util")]
pub const RECOVERY_FAILURE_ENV: &str = "POHUNEK_TEST_SERVICE_LOCK_RECOVERY_FAILURE";

fn recovery_call<T>(operation: &str, call: impl FnOnce() -> nix::Result<T>) -> nix::Result<T> {
    #[cfg(feature = "test-util")]
    if std::env::var(RECOVERY_FAILURE_ENV).is_ok_and(|value| value == operation) {
        return Err(nix::errno::Errno::EINTR);
    }
    let _ = operation;
    call()
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
        // A child that read the terminal while it was continued in the
        // background was stopped by `SIGTTIN`; it continues now that it may
        // read.
        let _ = nix::sys::signal::killpg(self.child, nix::sys::signal::Signal::SIGCONT);
        true
    }

    /// Takes the foreground back from the child's group, if it has it.
    ///
    /// The foreground is only taken while the child's group holds it, never
    /// from a shell that took it meanwhile.
    fn take_back(&self) {
        let stdin = std::io::stdin();
        if nix::unistd::tcgetpgrp(&stdin).ok() == Some(self.child) {
            claim_from_background(&stdin, self.own);
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
    /// Whether install would resolve the daemon `PATH`; the caller validates
    /// that after the job check, in install's order
    /// ([`engine::discover_for_plan`]).
    pub(crate) needs_discovery: bool,
    /// Whether install discovers before it looks for a daemon job: the
    /// rollback of an interrupted install does, a fresh install does not.
    pub(crate) discovery_first: bool,
    /// Whether the adoption preflight applies.
    pub(crate) adoption: Adoption,
    /// Pending upgrade whose rollback needs the previous reader's verdict.
    pub(crate) rollback: Option<record::Record>,
    /// The installed version whose sessions the adoption gate judges; the
    /// accepted-risk ledger applies only to it.
    pub(crate) installed_version: Option<String>,
}

/// Whether a check asks the new daemon's adoption preflight
/// ([`preflight::gate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Adoption {
    /// Upgrade swaps the daemon, so the live sessions are judged.
    Required,
    /// An install, a resumed upgrade, and a rerun of the active version swap
    /// no daemon.
    NotApplicable,
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
#[expect(
    clippy::too_many_lines,
    reason = "the check mirrors install and upgrade in transaction order; splitting it would separate the decisions from the state they read"
)]
pub(crate) fn check_local(
    context: &Context,
    prefix: Option<PathBuf>,
    version: &str,
    locked: bool,
) -> Result<Checked, Error> {
    let prefix = prefix
        .map(|prefix| absolute("--prefix", prefix))
        .transpose()?;
    let mut adoption = Adoption::NotApplicable;
    let mut installed_version = None;
    let config_path = context.config_path();
    let store = record::Store::new(context.paths().state_dir.clone());
    let pending = store.load()?;
    let pending_install = pending
        .as_ref()
        .is_some_and(|record| record.operation == record::Operation::Install);
    let (operation, prefix, namespace, plan, config_schema_migration, config_warnings) =
        if pending_install || !exists(&config_path)? {
            let prefix = match prefix {
                Some(prefix) => prefix,
                None => context.default_prefix()?,
            };
            let layout = engine::install_preflight(context, &prefix)?;
            let plan = engine::install_plan(pending, &prefix, version)?;
            // A resume past the config step reads the written file, so check proves
            // it exactly as install does.
            if let engine::Plan::Resume(record) = &plan {
                if record.step >= record::Step::Config {
                    engine::verify_resume_config(context, &prefix, version)?;
                }
            }
            // A rollback of the pending record removes the `service.toml` it wrote.
            if matches!(plan, engine::Plan::Fresh) && exists(&config_path)? {
                return Err(Error::AlreadyInstalled { path: config_path });
            }
            let namespace = context.namespace()?;
            layout::verify_claim(&layout, &namespace)?;
            (
                record::Operation::Install,
                prefix,
                namespace,
                plan,
                None,
                None,
            )
        } else {
            let config_source = verified_config_source(context)?;
            engine::upgrade_preflight(context)?;
            let plan = engine::upgrade_plan(pending, version)?;
            if let Some(record) = plan.record() {
                engine::verify_pending_migration(context, &store, record, &config_source)?;
            }
            let config = &config_source.config;
            engine::check_planned_renderings(config, version, &plan, &config_path)?;
            if engine::swaps_daemon(&plan, Some(config.active_version()), version) {
                adoption = Adoption::Required;
            }
            installed_version = Some(
                plan.record()
                    .and_then(|record| record.previous_version.clone())
                    .unwrap_or_else(|| config.active_version().to_owned()),
            );
            let layout = config.layout();
            if let Some(requested) = prefix.filter(|requested| requested != layout.prefix()) {
                return Err(Error::PrefixMismatch {
                    requested,
                    installed: layout.prefix().to_path_buf(),
                });
            }
            engine::check_layout_dirs(context, layout)?;
            layout::verify_claim(layout, &config.namespace())?;
            // A previous-schema file means the checked upgrade is a migration: it stops
            // the daemon before it rewrites `service.toml`.
            let config_schema_migration = checked_config_migration(&plan, &config_source, version);
            (
                record::Operation::Upgrade,
                layout.prefix().to_path_buf(),
                config.namespace(),
                plan,
                config_schema_migration,
                definition::unknown_keys_warning(config),
            )
        };
    let fresh_install =
        operation == record::Operation::Install && !matches!(plan, engine::Plan::Resume(_));
    let rollback = match &plan {
        engine::Plan::RollBack(record) if engine::rollback_restarts_previous(record) => {
            Some(record.clone())
        }
        _ => None,
    };
    let warnings = config_warnings.into_iter().collect();
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
        preflight: None,
        accepted_runtime_loss: false,
        acceptance: report::AcceptanceGroups::default(),
        config_schema_migration,
        warnings,
    };
    Ok(Checked {
        report,
        fresh_install,
        needs_discovery: operation == record::Operation::Install && engine::plan_discovers(&plan),
        discovery_first: matches!(plan, engine::Plan::RollBack(_)),
        adoption,
        rollback,
        installed_version,
    })
}

/// Reports a pending migration even after it has written the new schema.
fn checked_config_migration(
    plan: &engine::Plan,
    source: &UpgradeSource,
    version: &str,
) -> Option<u32> {
    match plan {
        engine::Plan::Resume(record) => record
            .config_backup
            .as_ref()
            .map(|backup| backup.source_schema_version),
        engine::Plan::RollBack(record)
            if record.config_backup.is_some()
                && record.previous_version.as_deref() != Some(version) =>
        {
            record
                .config_backup
                .as_ref()
                .map(|backup| backup.source_schema_version)
        }
        engine::Plan::Fresh if source.config.active_version() != version => {
            (source.source_schema_version != pohunek_service_config::SCHEMA_VERSION)
                .then_some(source.source_schema_version)
        }
        _ => None,
    }
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
    if checked.needs_discovery && checked.discovery_first {
        context.install_search_path()?;
    }
    if checked.fresh_install {
        engine::ensure_no_daemon_job(backend).await?;
    }
    if checked.needs_discovery && !checked.discovery_first {
        context.install_search_path()?;
    }
    Ok(checked.report)
}

/// Runs [`check`] for `context` against `backend`, with `preflight` judging
/// adoption of the daemon staged in `daemon_dir`.
///
/// # Errors
///
/// See [`check`].
#[cfg(test)]
pub(crate) async fn check_with_preflight(
    context: &Context,
    daemon_dir: &Path,
    preflight: &dyn preflight::AdoptionPreflight,
    version: &str,
    accept_runtime_loss: bool,
) -> Result<report::CheckReport, Error> {
    let checked = check_local(context, None, version, false)?;
    finish_check(
        checked,
        preflight,
        context,
        None,
        daemon_dir,
        version,
        accept_runtime_loss,
    )
    .await
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

/// Reports the installation `context` describes, as [`status`] does for the
/// process environment.
///
/// # Errors
///
/// Returns the errors of [`status`].
pub async fn status_in(context: &Context) -> Result<report::StatusReport, Error> {
    status_of(context).await
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
            pending_transaction: store
                .load()?
                .map(|pending| engine::pending_report(&pending)),
            warnings: Vec::new(),
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

/// The installation facts a plan reads, without side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledFacts {
    /// Whether `service.toml` records an installation.
    pub(crate) installed: bool,
    /// The recorded active version, when installed.
    pub(crate) active_version: Option<String>,
    /// The recorded install prefix, when installed.
    pub(crate) prefix: Option<PathBuf>,
    /// The interrupted transaction whose record is pending, if any.
    pub(crate) pending_transaction: Option<report::PendingReport>,
}

/// Reads whether the installation `context` describes is installed, at which
/// version and prefix, and whether a transaction record is pending.
///
/// Unlike [`status_in`] it only reads files: it probes no transaction lock,
/// contacts no service manager or daemon, and creates nothing, so a preview
/// can call it without touching the host. `service.toml` and the versions
/// directory are verified as `status` verifies them.
///
/// # Errors
///
/// Returns [`Error::Config`] for a `service.toml` that does not describe
/// this process's installation, [`Error::Record`] for a malformed record,
/// and a filesystem error for an unsafe state directory.
pub(crate) fn installed_facts(context: &Context) -> Result<InstalledFacts, Error> {
    let store = record::Store::new(context.paths().state_dir.clone());
    let pending_transaction = store
        .load()?
        .map(|pending| engine::pending_report(&pending));
    if !exists(&context.config_path())? {
        return Ok(InstalledFacts {
            installed: false,
            active_version: None,
            prefix: None,
            pending_transaction,
        });
    }
    let config = verified_config(context)?;
    // The versions directory is opened as the owner's trusted directory, as
    // `status` opens it, so an untrusted one fails before a plan runs the
    // installed daemon.
    layout::installed_versions(config.layout())?;
    Ok(InstalledFacts {
        installed: true,
        active_version: Some(config.active_version().to_owned()),
        prefix: Some(config.prefix().to_path_buf()),
        pending_transaction,
    })
}

pub(crate) fn staged_dir(context: &Context, from: Option<PathBuf>) -> Result<PathBuf, Error> {
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
/// The read is the upgrade-source read: a previous release's schema-2,
/// schema-3, schema-4 or schema-5 installation is reported, connected to,
/// checked, and uninstalled by this version like a current one, while schemas
/// this binary neither writes nor upgrades from stay refused.
///
/// # Errors
///
/// See [`verified_connection`].
fn verified_config(context: &Context) -> Result<ServiceConfig, Error> {
    Ok(verified_config_source(context)?.config)
}

/// Like [`verified_config`], and reports how `service.toml` was read.
///
/// # Errors
///
/// See [`verified_connection`].
fn verified_config_source(context: &Context) -> Result<UpgradeSource, Error> {
    let source = ServiceConfig::load_upgrade_source(&context.config_path())?;
    source.config.verify_installation(
        context.uid(),
        &context.paths().state_dir,
        &context.paths().runtime_dir,
    )?;
    Ok(source)
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
    fn check_reports_a_pending_rollback_migration_only_when_it_would_upgrade_again() {
        let (_temp, root, context, _state) = installed();
        let source = verified_config_source(&context).expect("current config");
        let record = record::Record {
            schema_version: record::SCHEMA_VERSION,
            operation: record::Operation::Upgrade,
            version: "2.0.0".to_owned(),
            prefix: root.join("home/.local"),
            previous_version: Some("1.0.0".to_owned()),
            version_dir_preexisted: false,
            config_backup: Some(record::ConfigBackup {
                source_schema_version: 2,
                digest: String::new(),
            }),
            step: record::Step::Registered,
            rolling_back: false,
            runtime_stop: false,
            runtime_backup: None,
            launchd_default_path: None,
            launchd_boot_identity: None,
            launchd_persisted_path: None,
            accepted_risks: None,
        };
        assert_eq!(
            checked_config_migration(&engine::Plan::Resume(record.clone()), &source, "2.0.0"),
            Some(2)
        );
        assert_eq!(
            checked_config_migration(&engine::Plan::RollBack(record.clone()), &source, "3.0.0"),
            Some(2)
        );
        assert_eq!(
            checked_config_migration(&engine::Plan::RollBack(record), &source, "1.0.0"),
            None
        );
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
