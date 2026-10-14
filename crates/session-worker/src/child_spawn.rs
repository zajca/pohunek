//! Starts a PTY child with the launch contract the worker has always provided.
//!
//! The child inherits the PTY slave as stdin, stdout, and stderr, becomes a
//! session leader with the slave as its controlling terminal, and receives the
//! environment and program resolution that `portable-pty` 0.9 gives its
//! children. The working directory is enforced by `std::process::Command`
//! itself: the chdir runs in the forked child before exec, and a failure is
//! returned from `spawn` with nothing executed.

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Stdio};

use nix::unistd::{access, AccessFlags, User};
use pohunek_worker_protocol::is_denylisted;
use rustix::fs::{open, Mode, OFlags};

use crate::pty::{Command, EnvBase};

/// Shell used when the password database has no usable login shell.
const FALLBACK_SHELL: &str = "/bin/sh";
/// Environment prefix reserved for pohunek-owned identity markers.
const RESERVED_ENV_PREFIX: &str = "POHUNEK_";
/// First descriptor the child marks close-on-exec; 0 through 2 are the slave.
const FIRST_INHERITED_DESCRIPTOR: libc::c_int = 3;
/// Signals whose inherited dispositions the child resets to the default.
const RESET_SIGNALS: [libc::c_int; 6] = [
    libc::SIGCHLD,
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGALRM,
];

/// Spawns `command` with `slave_device` as its stdin, stdout, and stderr.
///
/// The child's working directory is set by `std::process::Command::current_dir`,
/// so a missing directory fails the spawn before any program runs. The caller
/// must drop its own slave descriptors after this returns, since the master
/// observes end-of-file only when no slave descriptor remains open.
///
/// # Errors
///
/// Returns an error when the program cannot be resolved, the descriptor limit
/// cannot be read, the slave cannot be opened, or the child fails to start.
pub(crate) fn spawn_pty_child(command: &Command, slave_device: &Path) -> io::Result<Child> {
    let environment = child_environment(command);
    let program = resolve_program(
        Path::new(&command.program),
        &command.cwd,
        environment.get(OsStr::new("PATH")).map(OsString::as_os_str),
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "program is not executable"))?;
    // Computed in the parent because the child may not read the descriptor
    // table or call getrlimit after fork.
    let sweep_bound = descriptor_sweep_bound()?;

    // The slave is opened by name so that it never becomes the worker's
    // controlling terminal; `O_CLOEXEC` keeps it out of any other child.
    let slave = open(
        slave_device,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut process = std::process::Command::new(&program);
    process.arg0(&command.program);
    process.args(&command.args);
    process.current_dir(&command.cwd);
    process.env_clear();
    process.envs(&environment);
    process.stdin(Stdio::from(rustix::io::dup(&slave)?));
    process.stdout(Stdio::from(rustix::io::dup(&slave)?));
    process.stderr(Stdio::from(slave));
    install_session_setup(&mut process, sweep_bound);
    // The parent's slave descriptors close when `process` drops at the end of
    // this function, after the child has been created.
    process.spawn()
}

/// Builds the child environment as `portable-pty` 0.9 does.
///
/// `EnvBase::Empty` starts from nothing. `EnvBase::Inherited` starts from the
/// worker's environment without `POHUNEK_*` markers or denylisted names.
/// `command.env` is applied last, and `SHELL` is always present.
fn child_environment(command: &Command) -> BTreeMap<OsString, OsString> {
    let mut environment: BTreeMap<OsString, OsString> = match command.base {
        EnvBase::Empty => BTreeMap::new(),
        EnvBase::Inherited => std::env::vars_os()
            .filter(|(name, _)| {
                !matches!(name.to_str(), Some(name)
                    if name.starts_with(RESERVED_ENV_PREFIX) || is_denylisted(name))
            })
            .collect(),
    };
    for (key, value) in &command.env {
        environment.insert(OsString::from(key), OsString::from(value));
    }
    // The value `portable-pty` produces is the `SHELL` from `command.env` or the
    // worker environment when present, kept even when it is not executable.
    // Otherwise it is the login shell. `portable-pty`'s `get_shell` is private,
    // so `login_shell` repeats its passwd and `/bin/sh` fallback.
    let shell = environment
        .get(OsStr::new("SHELL"))
        .cloned()
        .unwrap_or_else(login_shell);
    environment.insert(OsString::from("SHELL"), shell);
    environment
}

/// Returns the account's login shell when it is usable, else the fallback shell.
fn login_shell() -> OsString {
    User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.shell.into_os_string())
        .filter(|shell| {
            shell.to_str().is_some() && access(Path::new(shell), AccessFlags::X_OK).is_ok()
        })
        .unwrap_or_else(|| OsString::from(FALLBACK_SHELL))
}

/// Resolves the executable the way `portable-pty` 0.9 does.
///
/// A path starting with `.` or `..` is relative to `cwd`; any other relative
/// name is searched in each `PATH` entry, itself relative to `cwd`. Absolute
/// paths are used as given. Directories never match.
fn resolve_program(program: &Path, cwd: &Path, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_absolute() {
        return is_runnable(program).then(|| program.to_path_buf());
    }
    if matches!(
        program.components().next(),
        Some(Component::CurDir | Component::ParentDir)
    ) {
        let candidate = cwd.join(program);
        return is_runnable(&candidate).then_some(candidate);
    }
    std::env::split_paths(path_var?)
        .map(|directory| cwd.join(directory).join(program))
        .find(|candidate| is_runnable(candidate))
}

/// Reports whether `path` is an executable that is not a directory.
fn is_runnable(path: &Path) -> bool {
    !path.is_dir() && access(path, AccessFlags::X_OK).is_ok()
}

/// Extra descriptor numbers the sweep covers above the highest listed one.
///
/// Descriptors opened by other threads between the listing and the fork take
/// the lowest free numbers, so they land just above the highest listed one.
/// The worker opens its descriptors close-on-exec, so this only guards against
/// non-close-on-exec opens that race the fork.
const SWEEP_SLACK: libc::c_int = 256;

/// Computes the exclusive upper bound of the child's fallback sweep, before fork.
///
/// The bound follows the descriptors this process holds open, not its soft
/// `RLIMIT_NOFILE`. A runtime that raises the soft limit to `RLIM_INFINITY`
/// would otherwise make the child loop over billions of numbers before exec.
/// Lowering the soft limit does not close descriptors already open above it,
/// and those are part of the listing.
///
/// # Errors
///
/// Returns an error when the descriptor table cannot be listed completely. The
/// spawn then fails rather than guessing a bound.
fn descriptor_sweep_bound() -> io::Result<libc::c_int> {
    Ok(highest_open_descriptor()?
        .saturating_add(1)
        .saturating_add(SWEEP_SLACK))
}

/// Returns the highest open descriptor number, or one below the inherited
/// range when none is open there. The listing is complete or an error.
#[cfg(target_os = "linux")]
fn highest_open_descriptor() -> io::Result<libc::c_int> {
    highest_listed_descriptor("/proc/self/fd")
}

/// Returns the highest open descriptor number, or one below the inherited
/// range when none is open there. The listing is complete or an error.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn highest_open_descriptor() -> io::Result<libc::c_int> {
    highest_listed_descriptor("/dev/fd")
}

/// Extra entries requested beyond the size probe when listing Darwin descriptors.
///
/// Another thread may open descriptors between the size probe and the listing.
/// A completely filled buffer is retried with double the room.
#[cfg(target_os = "macos")]
const DESCRIPTOR_LIST_SLACK: usize = 16;

/// Converts a `proc_pidinfo` result into a byte count.
///
/// `proc_pidinfo` reports failure as zero (or a negative value) with `errno`
/// set. The Rust runtime keeps descriptors 0 through 2 open (it reopens them
/// on `/dev/null` at startup when closed), so a zero-byte listing is never a
/// complete table and is treated as the failure it is.
#[cfg(target_os = "macos")]
fn listed_bytes(result: libc::c_int) -> io::Result<usize> {
    match usize::try_from(result) {
        Ok(bytes) if bytes > 0 => Ok(bytes),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Returns the highest open descriptor number, or one below the inherited
/// range when none is open there. The listing is complete or an error.
///
/// Uses `proc_pidinfo(PROC_PIDLISTFDS)`, the same descriptor-table query the
/// CLI uses for its bounded children.
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "proc_pidinfo(PROC_PIDLISTFDS) is the libc boundary for the descriptor table"
)]
fn highest_open_descriptor() -> io::Result<libc::c_int> {
    let pid = libc::c_int::try_from(std::process::id())
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidInput, source))?;
    let entry_size = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer with zero size only asks for the required byte count.
    let required =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let required = listed_bytes(required)?;
    let mut capacity = required / entry_size + DESCRIPTOR_LIST_SLACK;
    loop {
        let mut entries = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            capacity
        ];
        let buffer_size = capacity
            .checked_mul(entry_size)
            .ok_or_else(listing_overflow)?;
        let buffer_bytes =
            libc::c_int::try_from(buffer_size).map_err(|_source| listing_overflow())?;
        // SAFETY: `entries` provides `buffer_bytes` writable bytes of properly
        // aligned `proc_fdinfo` storage that outlives the call.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                buffer_bytes,
            )
        };
        let written = listed_bytes(written)?;
        if written < buffer_size {
            return Ok(entries[..written / entry_size]
                .iter()
                .map(|entry| entry.proc_fd)
                .max()
                .unwrap_or(FIRST_INHERITED_DESCRIPTOR - 1));
        }
        // A completely filled buffer may be truncated; retry with more room.
        capacity = capacity.checked_mul(2).ok_or_else(listing_overflow)?;
    }
}

/// Error for a descriptor listing whose buffer size overflows.
#[cfg(target_os = "macos")]
fn listing_overflow() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "open descriptor listing overflowed",
    )
}

/// Returns the highest number named in a directory of open descriptors.
#[cfg(not(target_os = "macos"))]
fn highest_listed_descriptor(directory: &str) -> io::Result<libc::c_int> {
    let mut highest = FIRST_INHERITED_DESCRIPTOR - 1;
    for entry in std::fs::read_dir(directory)? {
        let number = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::c_int>().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "open descriptor listing is not numeric",
                )
            })?;
        highest = highest.max(number);
    }
    Ok(highest)
}

/// Registers the child-side session setup that runs between fork and exec.
#[expect(
    unsafe_code,
    reason = "pre_exec is an unsafe API; the registered closure only calls async-signal-safe libc functions"
)]
fn install_session_setup(process: &mut std::process::Command, sweep_bound: libc::c_int) {
    // SAFETY: the closure captures one `c_int`, and calls only
    // `enter_pty_session`, which allocates nothing and calls only
    // async-signal-safe functions. That is the requirement after fork in a
    // multithreaded process.
    unsafe {
        process.pre_exec(move || enter_pty_session(sweep_bound));
    }
}

/// Makes the forked child a session leader on the PTY, in the order
/// `portable-pty` 0.9 uses in its pre-exec hook.
///
/// This runs after fork, so it allocates nothing and returns only the errno of
/// a failed call.
#[expect(
    unsafe_code,
    reason = "session setup and descriptor marking are libc calls with no safe wrapper"
)]
fn enter_pty_session(sweep_bound: libc::c_int) -> io::Result<()> {
    for signal in RESET_SIGNALS {
        // SAFETY: `signal` with `SIG_DFL` is async-signal-safe; the previous
        // disposition is intentionally discarded.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
    // SAFETY: a zeroed set is a valid `sigset_t`, `sigemptyset` initializes it,
    // and `sigprocmask` only reads it.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&raw mut empty) != 0
            || libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, std::ptr::null_mut()) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: `setsid` takes no pointers and only changes this child's session.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    // The request type differs by target: Linux declares `TIOCSCTTY` as the
    // type `ioctl` takes, while Darwin declares a `c_uint` and `ioctl` a
    // `c_ulong`.
    #[cfg(target_os = "linux")]
    let request = libc::TIOCSCTTY;
    #[cfg(target_os = "macos")]
    let request = libc::c_ulong::from(libc::TIOCSCTTY);
    // SAFETY: descriptor 0 is the PTY slave that std installed before this hook
    // runs, and `TIOCSCTTY` takes it as the controlling terminal.
    if unsafe { libc::ioctl(libc::STDIN_FILENO, request, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    mark_inherited_descriptors_cloexec(sweep_bound);
    Ok(())
}

/// Marks every descriptor from `FIRST_INHERITED_DESCRIPTOR` below `sweep_bound`
/// close-on-exec.
///
/// Marking instead of closing is required: std's exec-status pipe is a
/// close-on-exec descriptor above 2 that the child writes an exec failure
/// into, and std's child aborts when that write fails. The exec'd program sees
/// the same descriptors either way.
#[expect(
    unsafe_code,
    reason = "close_range and fcntl are libc calls with no safe wrapper"
)]
fn mark_inherited_descriptors_cloexec(sweep_bound: libc::c_int) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `syscall` with `SYS_close_range` takes integer arguments and
        // only updates the descriptor table. `CLOSE_RANGE_CLOEXEC` marks rather
        // than closes. The call fails before kernel 5.11, and the sweep below
        // then covers the range.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                libc::c_long::from(FIRST_INHERITED_DESCRIPTOR),
                libc::c_long::from(u32::MAX),
                libc::c_long::from(libc::CLOSE_RANGE_CLOEXEC),
            )
        };
        if marked == 0 {
            return;
        }
    }
    for descriptor in FIRST_INHERITED_DESCRIPTOR..sweep_bound {
        // SAFETY: `fcntl` with `F_SETFD` is async-signal-safe. Descriptors that
        // are not open return EBADF, which is expected and ignored.
        unsafe {
            libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}
