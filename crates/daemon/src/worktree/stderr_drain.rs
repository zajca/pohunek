//! Hand-off of a hook's stderr pipe to a detached drain process.
//!
//! A process a hook leaves running in the background may keep the hook's
//! stderr open for as long as it lives. When the daemon holds the pipe's read
//! end, that process gets `SIGPIPE` on its next stderr write once the daemon
//! exits, which a launchd job or an unsupervised daemon does without ending
//! the hook's process group. The drain takes the read end over instead: it is
//! `/bin/cat` with the pipe as stdin and `/dev/null` as stdout and stderr, in a
//! process group of its own, so it outlives a daemon restart exactly as the
//! hook's descendant does and ends at EOF, when the last writer is gone.
//!
//! The drain inherits nothing but its three standard descriptors: in the child,
//! before `exec`, every descriptor from 3 upward is marked close-on-exec.
//! Marking instead of closing keeps the standard library's exec-error pipe
//! working, so a failed `exec` is a spawn error the caller can fall back from.

// Rust guideline compliant 2026-10-15

use std::io::{self, PipeReader};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The drain program, by absolute path only: it is never looked up on `PATH`.
const DRAIN_PROGRAM: &str = "/bin/cat";

/// Descriptors 0, 1 and 2 are the drain's stdio; every higher one is marked
/// close-on-exec in the child.
const FIRST_INHERITED_FD: libc::c_int = 3;

/// Largest exclusive bound of the fallback marking loop.
///
/// The soft `RLIMIT_NOFILE` can be unlimited; no descriptor number reaches this
/// value (Darwin caps a process at `kern.maxfilesperproc`, Linux at
/// `fs.nr_open`, default 1 048 576).
const MAX_DESCRIPTOR_SCAN: libc::c_int = 1 << 20;

/// Entries requested beyond the size probe when listing Darwin descriptors;
/// another thread may open descriptors between the probe and the listing.
#[cfg(target_os = "macos")]
const DESCRIPTOR_LIST_SLACK: usize = 16;

/// The superuser, the only owner a trusted program and its directories have.
const ROOT_UID: u32 = 0;

/// Group and other write bits; a trusted entry has neither.
const UNSAFE_WRITE_BITS: u32 = 0o022;

/// A root-owned sticky directory may be writable by others: no other account
/// can replace an entry it does not own.
const STICKY_BIT: u32 = 0o1000;

/// Any execute bit.
const EXECUTE_BITS: u32 = 0o111;

/// Starts a drain that reads `pipe` until EOF and discards what it reads.
///
/// The drain gets a duplicate of the read end; the caller closes its own once
/// the drain runs. The read end is switched to blocking mode for the drain,
/// which affects every duplicate; on an error it is switched back, so the
/// caller can keep reading it without blocking.
///
/// # Errors
///
/// Returns the error of checking [`DRAIN_PROGRAM`], duplicating the read end,
/// changing its mode, or spawning the drain, including a failed `exec`.
pub(super) fn spawn_drain(pipe: &PipeReader) -> io::Result<Child> {
    let program = trusted_program(Path::new(DRAIN_PROGRAM))?;
    let read_end = pipe.try_clone()?;
    rustix::io::ioctl_fionbio(&read_end, false)?;
    let scan_end = descriptor_scan_end();
    let mut command = Command::new(program);
    command
        .stdin(Stdio::from(read_end))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .current_dir("/")
        .process_group(0);
    #[expect(
        unsafe_code,
        reason = "pre_exec is required to keep the daemon's descriptors out of the drain"
    )]
    // SAFETY: the closure calls only async-signal-safe descriptor syscalls
    // (`close_range`, `fcntl`) with scalar arguments.
    unsafe {
        command.pre_exec(move || {
            mark_inherited_cloexec(scan_end);
            Ok(())
        })
    };
    command.spawn().inspect_err(|_spawn| {
        // The caller keeps discarding in the daemon, which polls the pipe.
        let _ = rustix::io::ioctl_fionbio(pipe, true);
    })
}

/// Returns the canonical path of `program` when it is a root-owned executable
/// regular file that no other account can write, reached only through
/// root-owned directories no other account can write (a root-owned sticky
/// directory excepted).
///
/// # Errors
///
/// Returns the error of resolving or reading the path, or
/// [`io::ErrorKind::PermissionDenied`] when an entry is not trusted.
fn trusted_program(program: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(program)?;
    let metadata = std::fs::metadata(&canonical)?;
    let mode = metadata.permissions().mode();
    if !metadata.is_file()
        || metadata.uid() != ROOT_UID
        || mode & UNSAFE_WRITE_BITS != 0
        || mode & EXECUTE_BITS == 0
    {
        return Err(untrusted(&canonical));
    }
    for ancestor in canonical.ancestors().skip(1) {
        let metadata = std::fs::metadata(ancestor)?;
        let mode = metadata.permissions().mode();
        let shared_sticky = mode & STICKY_BIT != 0;
        if !metadata.is_dir()
            || metadata.uid() != ROOT_UID
            || (mode & UNSAFE_WRITE_BITS != 0 && !shared_sticky)
        {
            return Err(untrusted(ancestor));
        }
    }
    Ok(canonical)
}

fn untrusted(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} is not a root-owned entry closed to other accounts",
            path.display()
        ),
    )
}

/// The exclusive bound of the fallback marking loop, captured before fork.
///
/// It is the soft descriptor limit, at most [`MAX_DESCRIPTOR_SCAN`], raised
/// past the highest open descriptor: a limit lowered below a descriptor the
/// process still holds must not leave it out. When the descriptors cannot be
/// listed, the whole [`MAX_DESCRIPTOR_SCAN`] range is marked.
fn descriptor_scan_end() -> libc::c_int {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .and_then(|limit| libc::c_int::try_from(limit).ok())
        .map_or(MAX_DESCRIPTOR_SCAN, |limit| limit.min(MAX_DESCRIPTOR_SCAN));
    highest_open_descriptor().map_or(MAX_DESCRIPTOR_SCAN, |highest| {
        limit.max(highest.saturating_add(1))
    })
}

/// The highest open descriptor number of this process, from `/proc/self/fd`;
/// `None` when the directory cannot be read completely.
#[cfg(target_os = "linux")]
fn highest_open_descriptor() -> Option<libc::c_int> {
    let mut highest = FIRST_INHERITED_FD - 1;
    for entry in std::fs::read_dir("/proc/self/fd").ok()? {
        let descriptor: libc::c_int = entry.ok()?.file_name().to_str()?.parse().ok()?;
        highest = highest.max(descriptor);
    }
    Some(highest)
}

/// The highest open descriptor number of this process, from the kernel's
/// descriptor table; `None` when it cannot be listed completely.
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "proc_pidinfo(PROC_PIDLISTFDS) is the libc boundary for the descriptor table"
)]
fn highest_open_descriptor() -> Option<libc::c_int> {
    let pid = libc::c_int::try_from(std::process::id()).ok()?;
    let entry_size = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer with zero size only asks for the required byte count.
    let required =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let mut capacity = usize::try_from(required).ok().filter(|bytes| *bytes > 0)? / entry_size
        + DESCRIPTOR_LIST_SLACK;
    loop {
        let mut entries = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            capacity
        ];
        let buffer_bytes = libc::c_int::try_from(capacity.checked_mul(entry_size)?).ok()?;
        // SAFETY: `entries` provides `buffer_bytes` writable bytes of aligned
        // `proc_fdinfo` storage that outlives the call.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                buffer_bytes,
            )
        };
        let written = usize::try_from(written).ok().filter(|bytes| *bytes > 0)?;
        if written < capacity * entry_size {
            return entries[..written / entry_size]
                .iter()
                .map(|entry| entry.proc_fd)
                .max();
        }
        // A completely filled buffer may be truncated; retry with more room.
        capacity = capacity.checked_mul(2)?;
    }
}

/// Marks every descriptor from [`FIRST_INHERITED_FD`] up close-on-exec, using
/// only async-signal-safe syscalls.
///
/// Linux does it in one `close_range` call; the loop up to `scan_end` is the
/// path on Darwin and the fallback for kernels without `CLOSE_RANGE_CLOEXEC`.
fn mark_inherited_cloexec(scan_end: libc::c_int) {
    #[cfg(target_os = "linux")]
    {
        #[expect(
            unsafe_code,
            reason = "the raw close_range syscall is required inside pre_exec"
        )]
        // SAFETY: close_range receives scalar bounds and flags, no pointers.
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                FIRST_INHERITED_FD as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return;
        }
    }
    for descriptor in FIRST_INHERITED_FD..scan_end {
        #[expect(unsafe_code, reason = "raw fcntl calls are required inside pre_exec")]
        // SAFETY: fcntl accepts any integer descriptor; EBADF for a closed one
        // is harmless.
        unsafe {
            libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}
