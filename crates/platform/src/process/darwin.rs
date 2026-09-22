//! Darwin `libproc`, `sysctl`, and kqueue process inspection.
//!
//! Every fact comes from a kernel interface available to an unprivileged
//! same-user process. The inspector never shells out and never retains raw
//! argument or environment buffers beyond the decoding step.
//!
//! Darwin serves most process records only for the caller's own processes and
//! answers `EPERM` for anyone else's, so ownership is settled first through the
//! short BSD record, which the kernel serves regardless of who owns the target.
//! A privileged fact about a foreign process stays unreadable without the
//! `PRIV_GLOBAL_PROC_INFO` privilege that only root holds, and such a refusal is
//! reported as a denial rather than as absence.

// Rust guideline compliant 2026-09-22

use std::io;
use std::os::fd::AsFd;
use std::path::PathBuf;

use async_io::Async;

use super::darwin_layout::{
    bounded_descendants, children_by_parent, controlling_terminal_group, decode_command_name,
    decode_kernel_path, encode_boot_identity, encode_start_identity, parse_process_arguments,
    LayoutError,
};
use super::{
    BootIdentity, Error, ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector, StartIdentity,
};

/// Bounds consecutive empty kqueue drains after a readiness notification.
///
/// Exactly one filter is registered per watch, so a readable queue normally
/// yields the watched event. The bound turns an impossible readiness storm into
/// a typed failure instead of an unbounded wakeup loop.
const MAX_EMPTY_EXIT_DRAINS: usize = 1_024;

/// Darwin process inspector backed by `libproc`, `sysctl`, and kqueue.
#[derive(Debug, Clone, Copy, Default)]
pub struct DarwinInspector;

impl DarwinInspector {
    /// Creates a Darwin process inspector.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Returns the current opaque Darwin boot identity.
    ///
    /// The value encodes `kern.boottime`, so persisted same-boot process
    /// identities can be rejected after a reboot.
    ///
    /// # Errors
    ///
    /// Returns typed boot-identity inspection failures.
    pub fn boot_identity(&self) -> Result<BootIdentity, Error> {
        const OPERATION: &str = "read_boot_identity";

        let (seconds, microseconds) =
            native::boot_time().map_err(|source| facility_error(OPERATION, source))?;
        let value = encode_boot_identity(seconds, microseconds)
            .map_err(|error| layout_error(OPERATION, error))?;
        BootIdentity::parse(value)
    }
}

impl ProcessInspector for DarwinInspector {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
        read_identity(pid, native::effective_uid(), "inspect_process_identity")
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, Error> {
        const OPERATION: &str = "inspect_process_liveness";

        let Some(fact) = read_bsd_fact(identity.pid, native::effective_uid(), OPERATION)? else {
            return Ok(false);
        };
        Ok(fact.start_identity == identity.start_identity && !fact.is_zombie)
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, Error> {
        const OPERATION: &str = "inspect_process_parent";

        Ok(read_bsd_fact(pid, native::effective_uid(), OPERATION)?.map(|fact| fact.ppid))
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, Error> {
        read_process_fact(pid, native::effective_uid(), "inspect_process")
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
        const OPERATION: &str = "inspect_process_table";

        let euid = native::effective_uid();
        let pids = native::list_pids().map_err(|source| Error::from_io(OPERATION, source))?;
        let mut facts = Vec::new();
        for pid in pids {
            if let Some(fact) = read_process_fact(pid, euid, OPERATION)? {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, Error> {
        const OPERATION: &str = "inspect_descendants";

        let euid = native::effective_uid();
        let pids = descendant_pids(root, euid, OPERATION)?;
        let mut facts = Vec::with_capacity(pids.len());
        for pid in pids {
            if let Some(fact) = read_process_fact(pid, euid, OPERATION)? {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    fn descendant_identities(&self, root: ProcessIdentity) -> Result<Vec<ProcessIdentity>, Error> {
        const OPERATION: &str = "inspect_descendant_identities";

        // Reading identities only keeps lifecycle decisions independent of
        // argument regions, which an exec or a hardened image can withhold.
        let euid = native::effective_uid();
        require_identity(root, euid, OPERATION)?;
        let pids = descendant_pids(root.pid, euid, OPERATION)?;
        let mut identities = Vec::with_capacity(pids.len());
        for pid in pids {
            match read_identity(pid, euid, OPERATION) {
                Ok(Some(identity)) => identities.push(identity),
                Ok(None) | Err(Error::Race { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        require_identity(root, euid, OPERATION)?;
        Ok(identities)
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, Error> {
        const OPERATION: &str = "read_cwd";

        let raw = native::current_directory(native_pid(pid, OPERATION)?)
            .map_err(|source| process_error(OPERATION, source))?;
        decode_kernel_path(&raw).map_err(|error| layout_error(OPERATION, error))
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error> {
        const OPERATION: &str = "read_executable";

        let euid = native::effective_uid();
        if read_bsd_fact(pid, euid, OPERATION)?.is_none() {
            return Ok(None);
        }
        match native::executable_path(native_pid(pid, OPERATION)?) {
            Ok(path) => Ok(Some(path)),
            Err(error) if is_process_race(&error) => Ok(None),
            Err(source) => Err(Error::from_io(OPERATION, source)),
        }
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, Error> {
        const OPERATION: &str = "open_exit_watch";

        let euid = native::effective_uid();
        require_identity(identity, euid, OPERATION)?;
        let pid = native_pid(identity.pid, OPERATION)?;
        let queue =
            native::open_exit_queue(pid).map_err(|source| process_error(OPERATION, source))?;
        // A kqueue filter binds a process id, not an identity. Rechecking after
        // registration rejects a watch armed on a process that reused the id
        // between the first check and the registration.
        require_identity(identity, euid, OPERATION)?;
        let queue = Async::new_nonblocking(queue)
            .map_err(|source| Error::from_io("register_exit_watch", source))?;
        Ok(ExitWatch::from_future(async move {
            const WAIT_OPERATION: &str = "wait_for_exit";

            for _ in 0..MAX_EMPTY_EXIT_DRAINS {
                queue
                    .readable()
                    .await
                    .map_err(|source| Error::from_io(WAIT_OPERATION, source))?;
                match native::drain_exit_event(queue.get_ref().as_fd(), pid) {
                    Ok(native::ExitSignal::Exited) => return Ok(()),
                    Ok(native::ExitSignal::Pending) => {}
                    Err(source) => return Err(Error::from_io(WAIT_OPERATION, source)),
                }
            }
            Err(Error::InvalidData {
                operation: WAIT_OPERATION,
            })
        }))
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error> {
        const OPERATION: &str = "read_ownership_markers";

        let euid = native::effective_uid();
        let Some(fact) = read_bsd_fact(pid, euid, OPERATION)? else {
            return Ok(OwnershipMarkers::default());
        };
        match read_arguments(pid, &fact, OPERATION)? {
            Some(arguments) => Ok(arguments.markers),
            None => Ok(OwnershipMarkers::default()),
        }
    }

    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, Error> {
        const OPERATION: &str = "read_foreground_process_group";

        let Some(fact) = read_bsd_fact(root_pid, native::effective_uid(), OPERATION)? else {
            return Ok(None);
        };
        Ok(controlling_terminal_group(
            fact.terminal_device,
            fact.terminal_group,
        ))
    }
}

/// Kernel BSD process record reduced to the shared contract's vocabulary.
#[derive(Debug, Clone)]
struct BsdFact {
    ppid: Pid,
    pgid: Pid,
    comm: String,
    terminal_device: u32,
    terminal_group: u32,
    start_identity: StartIdentity,
    is_zombie: bool,
    is_exiting: bool,
}

/// Owner of one process id as reported by an unprivileged kernel read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// The process belongs to the calling effective user.
    SameUser,
    /// The process exists and belongs to another user.
    OtherUser,
    /// The process id no longer names a process the kernel will report.
    Gone,
}

/// Resolves who owns a process id without reading any privileged record.
///
/// Darwin serves the short BSD record regardless of who owns the target, so it
/// decides ownership before a call the kernel would refuse for a process the
/// caller does not own.
fn read_ownership(pid: Pid, euid: u32, operation: &'static str) -> Result<Ownership, Error> {
    match native::owner_info(native_pid(pid, operation)?) {
        Ok(info) if info.pid == pid && info.uid == euid => Ok(Ownership::SameUser),
        Ok(_) => Ok(Ownership::OtherUser),
        Err(error) if is_process_race(&error) => Ok(Ownership::Gone),
        Err(source) => Err(Error::from_io(operation, source)),
    }
}

/// Reads the BSD record of one same-user process.
///
/// Returns `None` for a process that exited or is owned by another user, which
/// preserves the same-user filter every caller depends on.
fn read_bsd_fact(pid: Pid, euid: u32, operation: &'static str) -> Result<Option<BsdFact>, Error> {
    if read_ownership(pid, euid, operation)? != Ownership::SameUser {
        return Ok(None);
    }
    let raw = match native::bsd_info(native_pid(pid, operation)?) {
        Ok(raw) => raw,
        Err(error) if is_process_race(&error) => return Ok(None),
        Err(error) if is_permission_denied(&error) => {
            // The kernel served this process id a moment ago, so a refusal now
            // means the process changed credentials or vanished. Ownership
            // separates that from a denial on a process the caller still owns,
            // which stays an explicit failure instead of an absence.
            return match read_ownership(pid, euid, operation)? {
                Ownership::SameUser => Err(Error::from_io(operation, error)),
                Ownership::OtherUser | Ownership::Gone => Ok(None),
            };
        }
        Err(source) => return Err(Error::from_io(operation, source)),
    };
    if raw.uid != euid || raw.pid != pid {
        return Ok(None);
    }
    Ok(Some(BsdFact {
        ppid: raw.ppid,
        pgid: raw.pgid,
        comm: decode_command_name(&raw.comm),
        terminal_device: raw.terminal_device,
        terminal_group: raw.terminal_group,
        start_identity: encode_start_identity(raw.start_seconds, raw.start_microseconds)
            .map_err(|error| layout_error(operation, error))?,
        is_zombie: raw.is_zombie,
        is_exiting: raw.is_exiting,
    }))
}

/// Reads the PID-reuse-safe identity of one same-user process.
fn read_identity(
    pid: Pid,
    euid: u32,
    operation: &'static str,
) -> Result<Option<ProcessIdentity>, Error> {
    Ok(
        read_bsd_fact(pid, euid, operation)?.map(|fact| ProcessIdentity {
            pid,
            start_identity: fact.start_identity,
        }),
    )
}

/// Fails unless the exact identity is still the process behind its id.
fn require_identity(
    expected: ProcessIdentity,
    euid: u32,
    operation: &'static str,
) -> Result<(), Error> {
    if read_identity(expected.pid, euid, operation)? == Some(expected) {
        Ok(())
    } else {
        Err(Error::Race { operation })
    }
}

/// Reads one process fact, discarding a record whose identity changed mid-read.
fn read_process_fact(
    pid: Pid,
    euid: u32,
    operation: &'static str,
) -> Result<Option<ProcessFact>, Error> {
    let Some(initial) = read_bsd_fact(pid, euid, operation)? else {
        return Ok(None);
    };
    let cmdline = read_arguments(pid, &initial, operation)?
        .map(|arguments| arguments.argv)
        .unwrap_or_default();
    let Some(current) = read_bsd_fact(pid, euid, operation)? else {
        return Ok(None);
    };
    if current.start_identity != initial.start_identity {
        return Ok(None);
    }
    Ok(Some(ProcessFact {
        pid,
        pgid: initial.pgid,
        ppid: initial.ppid,
        start_identity: initial.start_identity,
        comm: initial.comm,
        cmdline,
    }))
}

/// Reads and decodes the `KERN_PROCARGS2` regions of one same-user process.
///
/// Returns `None` for a process that holds no argument region: one that exited,
/// one retained as a zombie, one already inside `exit`, and one that has forked
/// but not yet published the region its `exec` will write. The caller has
/// established ownership, so the region being absent is the only thing `EINVAL`
/// can report, and whether the process itself exists stays a question for the
/// process record.
fn read_arguments(
    pid: Pid,
    fact: &BsdFact,
    operation: &'static str,
) -> Result<Option<super::darwin_layout::NativeArguments>, Error> {
    if fact.is_zombie || fact.is_exiting {
        return Ok(None);
    }
    let buffer = match native::process_arguments(native_pid(pid, operation)?) {
        Ok(buffer) => buffer,
        Err(error) if is_process_race(&error) => return Ok(None),
        Err(error) if is_missing_argument_region(&error) => return Ok(None),
        Err(source) => return Err(Error::from_io(operation, source)),
    };
    parse_process_arguments(&buffer)
        .map(Some)
        .map_err(|error| layout_error(operation, error))
}

/// Collects the bounded descendant set of one process from the same-user table.
fn descendant_pids(root: Pid, euid: u32, operation: &'static str) -> Result<Vec<Pid>, Error> {
    let pids = native::list_pids().map_err(|source| Error::from_io(operation, source))?;
    let mut table = Vec::with_capacity(pids.len());
    for pid in pids {
        if let Some(fact) = read_bsd_fact(pid, euid, operation)? {
            table.push((pid, fact.ppid));
        }
    }
    let index = children_by_parent(&table);
    bounded_descendants(root, |pid| {
        Ok::<_, Error>(index.get(&pid).cloned().unwrap_or_default())
    })
}

/// Converts a shared process id to the native signed representation.
fn native_pid(pid: Pid, operation: &'static str) -> Result<i32, Error> {
    i32::try_from(pid).map_err(|_range| Error::OutOfRange { operation })
}

/// Returns whether an error is a normal process-exit or exec race.
fn is_process_race(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ESRCH) || error.kind() == io::ErrorKind::NotFound
}

/// Returns whether `sysctl` reported that no argument region exists.
///
/// Darwin writes a process's argument region at the end of `exec` and releases
/// it on exit, and answers `EINVAL` whenever the region is not there.
fn is_missing_argument_region(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EINVAL)
}

/// Returns whether the kernel refused to serve the record at all.
fn is_permission_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

/// Classifies a process observation failure without exposing kernel buffers.
fn process_error(operation: &'static str, source: io::Error) -> Error {
    if is_process_race(&source) {
        Error::Race { operation }
    } else {
        Error::from_io(operation, source)
    }
}

/// Classifies a host-facility failure that means the interface is missing.
fn facility_error(operation: &'static str, source: io::Error) -> Error {
    if matches!(source.raw_os_error(), Some(libc::ENOENT | libc::ENOTSUP)) {
        Error::Unavailable { operation }
    } else {
        Error::from_io(operation, source)
    }
}

/// Maps a decoded-layout failure onto the shared typed error.
fn layout_error(operation: &'static str, error: LayoutError) -> Error {
    match error {
        LayoutError::Malformed => Error::InvalidData { operation },
        LayoutError::OutOfRange => Error::OutOfRange { operation },
    }
}

/// Wraps the Darwin process and event syscalls behind safe, bounded functions.
///
/// Nothing outside this module touches a raw pointer or a `libc` type; callers
/// receive owned Rust values or a typed `io::Error`.
#[expect(
    unsafe_code,
    reason = "libproc, sysctl, and kqueue have no safe Rust equivalent; each block documents its invariant"
)]
mod native {
    use std::ffi::OsString;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::ptr;

    use super::Pid;

    /// `PROC_ALL_PIDS` from `<sys/proc_info.h>`, which `libc` does not export.
    const PROC_ALL_PIDS: u32 = 1;

    /// `PROC_FLAG_INEXIT` from `<sys/proc_info.h>`, which `libc` does not export.
    ///
    /// The kernel sets this flag once a process enters `exit`, at which point it
    /// has already released the memory that holds its argument region.
    const PROC_FLAG_INEXIT: u32 = 4;

    /// Width of the kernel `pbi_comm` and `pbsi_comm` command-name fields.
    const COMMAND_NAME_BYTES: usize = 16;

    /// `PROC_PIDT_SHORTBSDINFO` from `<sys/proc_info.h>`, unexported by `libc`.
    const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;

    /// Width of `struct proc_bsdshortinfo`, which the kernel writes in full.
    const SHORT_RECORD_BYTES: usize = 64;

    /// Caps the process table read in one inventory pass.
    ///
    /// Darwin's `kern.maxproc` is well below this on every supported machine,
    /// so the bound only rejects an implausible kernel answer.
    const MAX_PROCESS_TABLE_ENTRIES: usize = 65_536;

    /// Extra slots so a table that grows between sizing and reading still fits.
    const PROCESS_TABLE_HEADROOM: usize = 128;

    /// Caps `KERN_PROCARGS2` buffers when the kernel does not report a size.
    ///
    /// Darwin bounds the combined argument and environment region by
    /// `kern.argmax`, whose documented maximum is 1 MiB.
    const MAX_ARGUMENT_BUFFER_BYTES: usize = 1024 * 1024;

    /// Caps kqueue events drained in one pass.
    ///
    /// One filter is registered per watch, so a small buffer always drains the
    /// queue completely.
    const MAX_DRAINED_EVENTS: usize = 8;

    /// Kernel BSD record copied out of `proc_pidinfo`.
    #[derive(Debug, Clone)]
    pub(super) struct RawBsdInfo {
        pub(super) pid: Pid,
        pub(super) ppid: Pid,
        pub(super) pgid: Pid,
        pub(super) uid: u32,
        pub(super) comm: [u8; COMMAND_NAME_BYTES],
        pub(super) terminal_device: u32,
        pub(super) terminal_group: u32,
        pub(super) start_seconds: u64,
        pub(super) start_microseconds: u64,
        pub(super) is_zombie: bool,
        pub(super) is_exiting: bool,
    }

    /// Short kernel record that identifies a process without any privilege.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct RawOwnerInfo {
        pub(super) pid: Pid,
        pub(super) uid: u32,
    }

    /// `struct proc_bsdshortinfo` from `<sys/proc_info.h>`, unexported by `libc`.
    ///
    /// The declaration mirrors the kernel field order and widths so the compiler
    /// computes the offsets. Fields the inspector does not read keep their
    /// kernel names behind an underscore because they carry the layout.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcBsdShortInfo {
        pbsi_pid: u32,
        _pbsi_ppid: u32,
        _pbsi_pgid: u32,
        _pbsi_status: u32,
        _pbsi_comm: [libc::c_char; COMMAND_NAME_BYTES],
        _pbsi_flags: u32,
        pbsi_uid: libc::uid_t,
        _pbsi_gid: libc::gid_t,
        _pbsi_ruid: libc::uid_t,
        _pbsi_rgid: libc::gid_t,
        _pbsi_svuid: libc::uid_t,
        _pbsi_svgid: libc::gid_t,
        _pbsi_rfu: u32,
    }

    // A transcription slip in the mirrored record must fail the build rather
    // than read a field from the wrong offset.
    const _: () = assert!(size_of::<ProcBsdShortInfo>() == SHORT_RECORD_BYTES);

    /// Outcome of draining one kqueue after a readiness notification.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ExitSignal {
        /// The kernel delivered `NOTE_EXIT` for the watched process.
        Exited,
        /// No matching event was pending, so the caller keeps waiting.
        Pending,
    }

    /// Returns the effective user id of the calling process.
    pub(super) fn effective_uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// Reads the short BSD process record for one process id.
    ///
    /// The kernel serves `PROC_PIDT_SHORTBSDINFO` regardless of who owns the
    /// target process, which is what makes it usable as an ownership probe.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, or `InvalidData` when the kernel
    /// wrote fewer bytes than the structure it documents.
    pub(super) fn owner_info(pid: i32) -> io::Result<RawOwnerInfo> {
        let size = buffer_length(size_of::<ProcBsdShortInfo>())?;
        let mut info = MaybeUninit::<ProcBsdShortInfo>::zeroed();
        // SAFETY: `proc_pidinfo` writes at most `size` bytes, and `info` is a
        // live, correctly aligned allocation of exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDT_SHORTBSDINFO,
                0,
                info.as_mut_ptr().cast::<libc::c_void>(),
                size,
            )
        };
        if written <= 0 {
            return Err(last_os_error());
        }
        if written != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short process record is shorter than the kernel structure",
            ));
        }
        // SAFETY: the kernel wrote the whole record, and the structure is plain
        // integer and character data, so every byte pattern it can write is a
        // valid value.
        let info = unsafe { info.assume_init() };
        Ok(RawOwnerInfo {
            pid: info.pbsi_pid,
            uid: info.pbsi_uid,
        })
    }

    /// Reads the BSD process record for one process id.
    ///
    /// The kernel serves `PROC_PIDTBSDINFO` only for the caller's own processes
    /// and answers `EPERM` for anyone else's.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, or `InvalidData` when the kernel
    /// wrote fewer bytes than the structure it documents.
    pub(super) fn bsd_info(pid: i32) -> io::Result<RawBsdInfo> {
        let size = buffer_length(size_of::<libc::proc_bsdinfo>())?;
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: `proc_pidinfo` writes at most `size` bytes, and `info` is a
        // live, correctly aligned allocation of exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast::<libc::c_void>(),
                size,
            )
        };
        if written <= 0 {
            return Err(last_os_error());
        }
        if written != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process record is shorter than the kernel structure",
            ));
        }
        // SAFETY: the kernel reported a complete `proc_bsdinfo`, and the
        // structure is plain integer and character data, so the zeroed bytes
        // outside it would also be a valid value.
        let info = unsafe { info.assume_init() };
        Ok(RawBsdInfo {
            pid: info.pbi_pid,
            ppid: info.pbi_ppid,
            pgid: info.pbi_pgid,
            uid: info.pbi_uid,
            comm: info.pbi_comm.map(i8::cast_unsigned),
            terminal_device: info.e_tdev,
            terminal_group: info.e_tpgid,
            start_seconds: info.pbi_start_tvsec,
            start_microseconds: info.pbi_start_tvusec,
            is_zombie: info.pbi_status == libc::SZOMB,
            is_exiting: info.pbi_flags & PROC_FLAG_INEXIT != 0,
        })
    }

    /// Reads the executable path of one process.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, or `InvalidData` for an empty path.
    pub(super) fn executable_path(pid: i32) -> io::Result<PathBuf> {
        let capacity = usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE)
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut buffer = vec![0_u8; capacity];
        let size = u32::try_from(capacity)
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `proc_pidpath` writes at most `size` bytes into `buffer`,
        // which owns exactly that many bytes.
        let written =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast::<libc::c_void>(), size) };
        if written <= 0 {
            return Err(last_os_error());
        }
        let written = usize::try_from(written)
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidData))?;
        if written > capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "executable path exceeds the kernel path bound",
            ));
        }
        buffer.truncate(written);
        Ok(PathBuf::from(OsString::from_vec(buffer)))
    }

    /// Reads the NUL-terminated current-directory field of one process.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, or `InvalidData` when the kernel
    /// wrote fewer bytes than the structure it documents.
    pub(super) fn current_directory(pid: i32) -> io::Result<Vec<u8>> {
        let size = buffer_length(size_of::<libc::proc_vnodepathinfo>())?;
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        // SAFETY: `proc_pidinfo` writes at most `size` bytes, and `info` is a
        // live, correctly aligned allocation of exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast::<libc::c_void>(),
                size,
            )
        };
        if written <= 0 {
            return Err(last_os_error());
        }
        if written != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "vnode path record is shorter than the kernel structure",
            ));
        }
        // SAFETY: the kernel reported a complete `proc_vnodepathinfo`, and the
        // structure is plain integer and character data.
        let info = unsafe { info.assume_init() };
        Ok(info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|byte| byte.cast_unsigned())
            .collect())
    }

    /// Lists every process id the kernel reports, bounded by the table cap.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, or `InvalidData` when the kernel
    /// reports a table larger than the bound.
    pub(super) fn list_pids() -> io::Result<Vec<Pid>> {
        // SAFETY: a null buffer with zero length asks `proc_listpids` for the
        // required byte count and writes nothing.
        let needed = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(last_os_error());
        }
        let entries = usize::try_from(needed)
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidData))?
            / size_of::<i32>()
            + PROCESS_TABLE_HEADROOM;
        if entries > MAX_PROCESS_TABLE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process table exceeds the supported bound",
            ));
        }
        let mut buffer = vec![0_i32; entries];
        let size = buffer_length(entries * size_of::<i32>())?;
        // SAFETY: `proc_listpids` writes at most `size` bytes into `buffer`,
        // which owns exactly that many bytes of correctly aligned `i32`s.
        let written = unsafe {
            libc::proc_listpids(
                PROC_ALL_PIDS,
                0,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                size,
            )
        };
        if written <= 0 {
            return Err(last_os_error());
        }
        let written = usize::try_from(written)
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidData))?
            / size_of::<i32>();
        if written > entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process table result exceeds the requested buffer",
            ));
        }
        Ok(buffer[..written]
            .iter()
            .filter_map(|raw| Pid::try_from(*raw).ok())
            .filter(|pid| *pid != 0)
            .collect())
    }

    /// Reads the raw `KERN_PROCARGS2` buffer of one process.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error. Darwin reports a vanished process
    /// and an unreadable argument region with the same `EINVAL`.
    pub(super) fn process_arguments(pid: i32) -> io::Result<Vec<u8>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut length: libc::size_t = 0;
        // SAFETY: `mib` is a live array of exactly the length passed, and a
        // null output buffer asks `sysctl` for the required size only.
        let probed = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib_length(&mib)?,
                ptr::null_mut(),
                &raw mut length,
                ptr::null_mut(),
                0,
            )
        };
        let capacity = if probed == 0 && length > 0 && length <= MAX_ARGUMENT_BUFFER_BYTES {
            length
        } else {
            MAX_ARGUMENT_BUFFER_BYTES
        };
        let mut buffer = vec![0_u8; capacity];
        let mut written: libc::size_t = capacity;
        // SAFETY: `buffer` owns exactly `written` bytes, and `sysctl` writes at
        // most that many and reports the count back through `written`.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib_length(&mib)?,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                &raw mut written,
                ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(last_os_error());
        }
        if written > capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "argument region result exceeds the requested buffer",
            ));
        }
        buffer.truncate(written);
        Ok(buffer)
    }

    /// Reads `kern.boottime` as whole seconds and microseconds.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, or `InvalidData` when the kernel
    /// wrote fewer bytes than the structure it documents.
    pub(super) fn boot_time() -> io::Result<(i64, i32)> {
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let mut boot = MaybeUninit::<libc::timeval>::zeroed();
        let mut length: libc::size_t = size_of::<libc::timeval>();
        // SAFETY: `mib` is a live array of exactly the length passed, and
        // `boot` is a live, correctly aligned allocation of `length` bytes.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib_length(&mib)?,
                boot.as_mut_ptr().cast::<libc::c_void>(),
                &raw mut length,
                ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(last_os_error());
        }
        if length != size_of::<libc::timeval>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "boot time record is shorter than the kernel structure",
            ));
        }
        // SAFETY: the kernel reported a complete `timeval` of integer fields.
        let boot = unsafe { boot.assume_init() };
        Ok((boot.tv_sec, boot.tv_usec))
    }

    /// Registers a one-shot `NOTE_EXIT` filter for one process id.
    ///
    /// The returned descriptor owns the registration: dropping it releases the
    /// kernel filter immediately.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error. A process that already exited is
    /// reported as `ESRCH`, never as an exit event.
    pub(super) fn open_exit_queue(pid: i32) -> io::Result<OwnedFd> {
        // SAFETY: `kqueue` takes no arguments and returns a fresh descriptor
        // or -1.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor owned by nothing else.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };

        let change = libc::kevent {
            ident: usize::try_from(pid)
                .map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))?,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT | libc::EV_RECEIPT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: ptr::null_mut(),
        };
        let mut receipt = [empty_event()];
        let immediate = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `queue` is a live kqueue descriptor, and the change and event
        // lists are live arrays of exactly the lengths passed to the call.
        let count = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &raw const change,
                1,
                receipt.as_mut_ptr(),
                1,
                &raw const immediate,
            )
        };
        if count < 0 {
            return Err(last_os_error());
        }
        // `EV_RECEIPT` forces one `EV_ERROR` result per change, whose `data`
        // field carries the registration errno and is zero on success.
        if count > 0 && receipt[0].flags & libc::EV_ERROR != 0 && receipt[0].data != 0 {
            let errno = i32::try_from(receipt[0].data).unwrap_or(libc::EIO);
            return Err(io::Error::from_raw_os_error(errno));
        }
        Ok(queue)
    }

    /// Drains one readable kqueue and reports whether the watched process exited.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error, including an `EV_ERROR` result the
    /// kernel attached to the registered filter.
    pub(super) fn drain_exit_event(queue: BorrowedFd<'_>, pid: i32) -> io::Result<ExitSignal> {
        let expected =
            usize::try_from(pid).map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut events = [empty_event(); MAX_DRAINED_EVENTS];
        let capacity = libc::c_int::try_from(events.len())
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))?;
        let immediate = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `queue` is a live kqueue descriptor, and `events` is a live
        // array of exactly `capacity` entries. No changes are submitted.
        let count = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                ptr::null(),
                0,
                events.as_mut_ptr(),
                capacity,
                &raw const immediate,
            )
        };
        if count < 0 {
            let error = last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(ExitSignal::Pending)
            } else {
                Err(error)
            };
        }
        let count = usize::try_from(count)
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidData))?
            .min(events.len());
        for event in &events[..count] {
            if event.filter != libc::EVFILT_PROC || event.ident != expected {
                continue;
            }
            if event.flags & libc::EV_ERROR != 0 {
                let errno = i32::try_from(event.data).unwrap_or(libc::EIO);
                return Err(io::Error::from_raw_os_error(errno));
            }
            if event.fflags & libc::NOTE_EXIT != 0 {
                return Ok(ExitSignal::Exited);
            }
        }
        Ok(ExitSignal::Pending)
    }

    /// Returns a kernel event with no registration or result fields set.
    const fn empty_event() -> libc::kevent {
        libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: ptr::null_mut(),
        }
    }

    /// Converts a byte count to the signed length the `libproc` calls take.
    fn buffer_length(bytes: usize) -> io::Result<libc::c_int> {
        libc::c_int::try_from(bytes).map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// Converts a management information base length to the `sysctl` width.
    fn mib_length(mib: &[libc::c_int]) -> io::Result<libc::c_uint> {
        libc::c_uint::try_from(mib.len())
            .map_err(|_range| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// Returns the current errno, or a typed failure when the kernel set none.
    ///
    /// The `libproc` calls report failure by returning a non-positive count,
    /// which is not always accompanied by an errno.
    fn last_os_error() -> io::Error {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(0) {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "kernel reported a failure without an error code",
            )
        } else {
            error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DarwinInspector, Error, ExitWatch, ProcessInspector};
    use crate::process::{Pid, ProcessIdentity, StartIdentity};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// Bounds every poll for an observable operating-system state change.
    const OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);
    /// Spacing between polls while waiting for an observable state change.
    const OBSERVE_POLL: Duration = Duration::from_millis(20);
    /// Bounds how long a killed pseudoterminal shell may take to become reapable.
    ///
    /// Shorter than the observation window so the fixture always answers before
    /// the caller that waits on it gives up.
    const REAP_TIMEOUT: Duration = Duration::from_secs(2);
    /// Proves a watch on a live process stays pending without busy polling.
    const IDLE_WATCH_WINDOW: Duration = Duration::from_millis(250);
    /// Number of concurrent idle watches armed on one live process.
    const IDLE_WATCH_COUNT: usize = 512;
    /// Short-lived processes spawned by the churn test.
    const CHURN_ROUNDS: usize = 64;
    /// Sentinel that must never reach an error, event, or log surface.
    const SECRET_SENTINEL: &str = "pohunek-darwin-secret-sentinel";

    /// Kills and reaps a spawned fixture even when an assertion fails.
    struct Fixture(Child);

    impl Fixture {
        fn pid(&self) -> Pid {
            self.0.id()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            // Reaping is bounded for the same reason the pseudoterminal fixture
            // bounds it: a child that never reports its exit would otherwise
            // stall every test that spawned one, with no diagnosis.
            let deadline = Instant::now() + REAP_TIMEOUT;
            while Instant::now() < deadline {
                if matches!(self.0.try_wait(), Ok(Some(_status)) | Err(_unreapable)) {
                    return;
                }
                std::thread::sleep(OBSERVE_POLL);
            }
        }
    }

    /// Kills and reaps a pseudoterminal shell even when an assertion fails.
    struct PtyFixture {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        pid: Pid,
    }

    impl PtyFixture {
        /// Kills the shell and reports whether it became reapable in time.
        ///
        /// The signal is sent directly because the pseudoterminal crate's own
        /// kill sends `SIGHUP` first and leaves an interactive shell deciding
        /// what to do with its jobs.
        fn reap(&mut self) -> bool {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(
                    i32::try_from(self.pid).expect("a shell pid fits a pid"),
                ),
                nix::sys::signal::Signal::SIGKILL,
            );
            let deadline = Instant::now() + REAP_TIMEOUT;
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_status)) => return true,
                    Ok(None) => std::thread::sleep(OBSERVE_POLL),
                    Err(_unreapable) => return false,
                }
            }
            false
        }
    }

    impl Drop for PtyFixture {
        fn drop(&mut self) {
            // A shell that never becomes reapable must not stall the suite; the
            // CI job terminates whatever is left when it ends.
            let _ = self.reap();
        }
    }

    /// Spawns a shell that stays alive until it is killed.
    fn spawn_idle_shell() -> Fixture {
        spawn_shell("trap '' TERM; while :; do sleep 1; done", &[], None)
    }

    /// Spawns `/bin/sh` with an explicit script, environment, and directory.
    fn spawn_shell(
        script: &str,
        environment: &[(&str, &str)],
        directory: Option<&Path>,
    ) -> Fixture {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in environment {
            command.env(name, value);
        }
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
        Fixture(command.spawn().expect("spawn shell fixture"))
    }

    /// Runs one observation on a helper thread and bounds how long it may take.
    ///
    /// A kernel call that never returns then fails this test with the phase that
    /// stalled, instead of stalling the whole suite until CI cancels the job.
    fn observe<T: Send + 'static>(what: &str, probe: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(probe());
        });
        receiver
            .recv_timeout(OBSERVE_TIMEOUT)
            .unwrap_or_else(|_elapsed| panic!("{what} never answered"))
    }

    /// Awaits one exit watch under an explicit deadline.
    ///
    /// An event-driven watch that never completes is a defect to be reported,
    /// not a reason for the suite to wait forever.
    async fn await_exit(what: &str, watch: ExitWatch) {
        tokio::time::timeout(OBSERVE_TIMEOUT, watch.wait())
            .await
            .unwrap_or_else(|_elapsed| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|error| panic!("unexpected watch failure for {what}: {error}"));
    }

    /// Polls until `probe` yields a value or the observation window elapses.
    fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + OBSERVE_TIMEOUT;
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(OBSERVE_POLL);
        }
    }

    /// Returns the exact identity of a live fixture.
    fn live_identity(inspector: DarwinInspector, pid: Pid) -> ProcessIdentity {
        wait_for("the fixture to become observable", || {
            inspector.identity(pid).expect("inspect fixture identity")
        })
    }

    #[test]
    fn self_facts_match_the_running_process() {
        let inspector = DarwinInspector::new();
        let pid = std::process::id();

        let fact = inspector
            .process(pid)
            .expect("inspect the current process")
            .expect("the current process is live");

        assert_eq!(fact.pid, pid);
        assert!(!fact.comm.is_empty(), "kernel command name must be present");
        assert!(
            !fact.cmdline.is_empty(),
            "the test binary always has an argument vector"
        );
        assert_eq!(
            inspector.identity(pid).expect("inspect identity"),
            Some(fact.identity())
        );
        assert!(inspector
            .is_running(fact.identity())
            .expect("inspect liveness"));
        assert_eq!(
            inspector.parent_pid(pid).expect("inspect parent"),
            Some(fact.ppid)
        );
    }

    #[test]
    fn the_process_table_keeps_only_same_user_processes() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_idle_shell();
        let pid = fixture.pid();

        let table = wait_for("the fixture to appear in the process table", || {
            let table = inspector
                .same_user_processes()
                .expect("inspect the process table");
            table.iter().any(|fact| fact.pid == pid).then_some(table)
        });

        assert!(
            table.iter().all(|fact| fact.pid != 1),
            "launchd is owned by root and must be filtered out"
        );
        assert!(table.iter().any(|fact| fact.pid == std::process::id()));
    }

    #[test]
    fn the_process_table_survives_concurrent_spawn_churn() {
        let inspector = DarwinInspector::new();
        let resident = spawn_idle_shell();
        let resident_pid = live_identity(inspector, resident.pid()).pid;

        // A process caught between its fork and the end of its exec holds no
        // argument region yet. That is a fact about that one process and must
        // neither fail the inventory nor drop a process that is plainly live.
        for _ in 0..CHURN_ROUNDS {
            let mut churn = spawn_shell("exit 0", &[], None);
            let table = inspector
                .same_user_processes()
                .expect("inspect the process table during churn");
            assert!(
                table.iter().any(|fact| fact.pid == resident_pid),
                "a live same-user process must stay in every inventory snapshot"
            );
            churn.0.wait().expect("reap the churn fixture");
        }
    }

    #[test]
    fn another_users_process_is_absent_or_denied_but_never_guessed() {
        /// `launchd` holds process id 1 and always runs as root.
        const ROOT_OWNED_PID: Pid = 1;

        let inspector = DarwinInspector::new();
        assert_ne!(
            super::native::effective_uid(),
            0,
            "a root caller reads every process and cannot observe the same-user filter"
        );

        assert!(
            inspector
                .identity(ROOT_OWNED_PID)
                .expect("inspect a root-owned identity")
                .is_none(),
            "a process owned by another user is outside the same-user contract"
        );
        assert!(inspector
            .process(ROOT_OWNED_PID)
            .expect("inspect a root-owned process")
            .is_none());
        assert!(inspector
            .parent_pid(ROOT_OWNED_PID)
            .expect("inspect a root-owned parent")
            .is_none());
        assert!(inspector
            .executable(ROOT_OWNED_PID)
            .expect("inspect a root-owned executable")
            .is_none());
        assert!(
            !inspector
                .ownership_markers(ROOT_OWNED_PID)
                .expect("inspect root-owned markers")
                .is_marked(),
            "markers are never read from a process outside the same-user contract"
        );
        assert!(
            matches!(
                inspector.cwd(ROOT_OWNED_PID),
                Err(Error::PermissionDenied { .. })
            ),
            "a privileged fact must be reported as a denial, never as absence or a race"
        );
    }

    #[test]
    fn descendants_follow_a_spawned_subtree() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_shell("sleep 60 & wait", &[], None);
        let root = live_identity(inspector, fixture.pid());

        let descendant = wait_for("the fixture to create its descendant", || {
            inspector
                .descendant_identities(root)
                .expect("inspect descendants")
                .into_iter()
                .next()
        });

        let facts = inspector
            .descendants(root.pid)
            .expect("inspect descendant facts");
        assert!(facts.iter().any(|fact| fact.pid == descendant.pid));
        assert!(
            inspector
                .descendant_identities(live_identity(inspector, std::process::id()))
                .expect("inspect transitive descendants")
                .iter()
                .any(|identity| identity.pid == descendant.pid),
            "a grandchild must appear in the transitive descendant set"
        );
    }

    #[test]
    fn descendant_identities_reject_a_substituted_root() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_idle_shell();
        let root = live_identity(inspector, fixture.pid());
        let substituted = ProcessIdentity {
            pid: root.pid,
            start_identity: StartIdentity::new(root.start_identity.get().wrapping_add(1)),
        };

        assert!(matches!(
            inspector.descendant_identities(substituted),
            Err(Error::Race { .. })
        ));
    }

    #[test]
    fn cwd_reports_the_child_working_directory() {
        let inspector = DarwinInspector::new();
        let directory = tempfile::tempdir().expect("create a working directory");
        let expected = directory
            .path()
            .canonicalize()
            .expect("canonicalize the working directory");
        let fixture = spawn_shell(
            "trap '' TERM; while :; do sleep 1; done",
            &[],
            Some(&expected),
        );
        let pid = fixture.pid();

        let observed = wait_for("the fixture working directory", || {
            match inspector.cwd(pid) {
                Ok(path) => Some(path),
                Err(Error::Race { .. }) => None,
                Err(error) => panic!("unexpected cwd failure: {error}"),
            }
        });

        assert_eq!(observed, expected);
    }

    #[test]
    fn cwd_preserves_exact_directory_bytes() {
        let inspector = DarwinInspector::new();
        let parent = tempfile::tempdir().expect("create a parent directory");
        let parent = parent
            .path()
            .canonicalize()
            .expect("canonicalize the parent directory");

        // Darwin filesystems validate filename bytes and reject an invalid
        // UTF-8 sequence with `EILSEQ`, so no such directory can exist to be
        // observed end to end. The kernel still hands out raw path bytes, and
        // `darwin_layout::tests::non_utf8_kernel_path_survives_decoding` pins
        // the decoder against bytes the filesystem refuses to store.
        let mut refused = b"agent-".to_vec();
        refused.extend_from_slice(&[0xC3, 0x28]);
        std::fs::create_dir(parent.join(PathBuf::from(OsString::from_vec(refused))))
            .expect_err("a non-UTF-8 directory name must stay uncreatable on this filesystem");

        // U+0416 CYRILLIC CAPITAL LETTER ZHE has no canonical decomposition, so
        // the stored name is byte-identical under every normalization rule a
        // Darwin filesystem applies.
        let expected = parent.join(PathBuf::from(OsString::from_vec(
            b"agent-\xD0\x96".to_vec(),
        )));
        std::fs::create_dir(&expected).expect("create a multi-byte directory");
        let fixture = spawn_shell(
            "trap '' TERM; while :; do sleep 1; done",
            &[],
            Some(&expected),
        );
        let pid = fixture.pid();

        let observed = wait_for("the multi-byte working directory", || {
            match inspector.cwd(pid) {
                Ok(path) => Some(path),
                Err(Error::Race { .. }) => None,
                Err(error) => panic!("unexpected cwd failure: {error}"),
            }
        });

        assert_eq!(observed, expected);
    }

    #[test]
    fn cwd_reports_a_race_after_the_process_exits() {
        let inspector = DarwinInspector::new();
        let mut fixture = spawn_shell("exit 0", &[], None);
        let pid = fixture.pid();
        fixture.0.wait().expect("reap the fixture");

        assert!(matches!(inspector.cwd(pid), Err(Error::Race { .. })));
    }

    #[test]
    fn executable_reports_the_spawned_image() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_idle_shell();
        let pid = fixture.pid();

        let executable = wait_for("the fixture executable path", || {
            inspector.executable(pid).expect("inspect executable")
        });

        assert_eq!(
            executable.file_name().and_then(|name| name.to_str()),
            Some("sh")
        );
    }

    #[test]
    fn executable_reports_none_for_an_exited_process() {
        let inspector = DarwinInspector::new();
        let mut fixture = spawn_shell("exit 0", &[], None);
        let pid = fixture.pid();
        fixture.0.wait().expect("reap the fixture");

        assert_eq!(inspector.executable(pid).expect("inspect executable"), None);
    }

    #[test]
    fn ownership_markers_describe_each_process_environment() {
        let inspector = DarwinInspector::new();
        let outer = spawn_shell(
            "POHUNEK_SESSION_ID=inner-session /bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' & wait",
            &[("POHUNEK_SESSION_ID", "outer-session"), ("POHUNEK_DAEMON_ID", "outer-daemon")],
            None,
        );
        let root = live_identity(inspector, outer.pid());

        let markers = inspector
            .ownership_markers(root.pid)
            .expect("inspect outer markers");
        assert_eq!(markers.session_id.as_deref(), Some("outer-session"));
        assert_eq!(markers.daemon_id.as_deref(), Some("outer-daemon"));

        let nested = wait_for("the nested fixture", || {
            inspector
                .descendant_identities(root)
                .expect("inspect nested descendants")
                .into_iter()
                .find(|identity| {
                    inspector
                        .ownership_markers(identity.pid)
                        .is_ok_and(|markers| markers.session_id.as_deref() == Some("inner-session"))
                })
        });

        let nested_markers = inspector
            .ownership_markers(nested.pid)
            .expect("inspect nested markers");
        assert_eq!(nested_markers.session_id.as_deref(), Some("inner-session"));
        assert_eq!(
            nested_markers.daemon_id.as_deref(),
            Some("outer-daemon"),
            "a nested process reports the markers it actually inherited"
        );
    }

    #[test]
    fn an_argument_that_looks_like_a_marker_is_not_a_marker() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_shell(
            "trap '' TERM; while :; do sleep 1; done # POHUNEK_SESSION_ID=argv-only",
            &[],
            None,
        );
        let pid = fixture.pid();

        let fact = wait_for("the fixture arguments", || {
            inspector.process(pid).expect("inspect the fixture")
        });

        assert!(fact
            .cmdline
            .iter()
            .any(|argument| argument.contains("POHUNEK_SESSION_ID=argv-only")));
        assert!(
            !inspector
                .ownership_markers(pid)
                .expect("inspect markers")
                .is_marked(),
            "an argument region entry must never become an ownership marker"
        );
    }

    #[test]
    fn no_secret_environment_value_reaches_facts_or_errors() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_shell(
            "trap '' TERM; while :; do sleep 1; done",
            &[("POHUNEK_TEST_SECRET", SECRET_SENTINEL)],
            None,
        );
        let pid = fixture.pid();

        let fact = wait_for("the fixture facts", || {
            inspector.process(pid).expect("inspect the fixture")
        });
        let markers = inspector.ownership_markers(pid).expect("inspect markers");
        let stale = ProcessIdentity {
            pid,
            start_identity: StartIdentity::new(0),
        };
        let race = inspector
            .exit_watch(stale)
            .expect_err("a substituted identity cannot arm a watch");

        for rendered in [
            format!("{fact:?}"),
            format!("{markers:?}"),
            format!("{race:?}"),
            race.to_string(),
        ] {
            assert!(
                !rendered.contains(SECRET_SENTINEL),
                "a secret sentinel leaked into `{rendered}`"
            );
        }
    }

    #[test]
    fn boot_identity_is_stable_within_one_boot() {
        let inspector = DarwinInspector::new();

        let first = inspector.boot_identity().expect("read boot identity");
        let second = inspector.boot_identity().expect("read boot identity");

        assert_eq!(first, second);
        assert!(!first.as_str().is_empty());
    }

    #[tokio::test]
    async fn exit_watch_completes_after_the_process_exits() {
        let inspector = DarwinInspector::new();
        let mut fixture = spawn_shell("sleep 60", &[], None);
        let identity = live_identity(inspector, fixture.pid());

        let watch = inspector.exit_watch(identity).expect("arm the exit watch");
        fixture.0.kill().expect("kill the fixture");

        await_exit("the fixture exit", watch).await;
        assert!(!inspector.is_running(identity).expect("inspect liveness"));
    }

    #[tokio::test]
    async fn every_watch_on_one_identity_completes_once_it_exits() {
        let inspector = DarwinInspector::new();
        let mut fixture = spawn_shell("sleep 60", &[], None);
        let identity = live_identity(inspector, fixture.pid());

        let first = inspector.exit_watch(identity).expect("arm the first watch");
        let second = inspector
            .exit_watch(identity)
            .expect("arm the second watch");
        fixture.0.kill().expect("kill the fixture");

        await_exit("the first watch", first).await;
        await_exit("the second watch", second).await;
    }

    #[tokio::test]
    async fn exit_watch_rejects_a_process_that_already_exited() {
        let inspector = DarwinInspector::new();
        let mut fixture = spawn_shell("exit 0", &[], None);
        let pid = fixture.pid();
        let identity = ProcessIdentity {
            pid,
            start_identity: StartIdentity::new(1),
        };
        fixture.0.wait().expect("reap the fixture");

        assert!(
            matches!(inspector.exit_watch(identity), Err(Error::Race { .. })),
            "a reaped process id is a race, never an exit event"
        );
    }

    #[tokio::test]
    async fn exit_watch_rejects_a_substituted_start_identity() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_idle_shell();
        let live = live_identity(inspector, fixture.pid());
        let substituted = ProcessIdentity {
            pid: live.pid,
            start_identity: StartIdentity::new(live.start_identity.get().wrapping_add(1)),
        };

        assert!(matches!(
            inspector.exit_watch(substituted),
            Err(Error::Race { .. })
        ));
    }

    #[tokio::test]
    async fn short_lived_process_churn_never_reports_a_false_exit() {
        let inspector = DarwinInspector::new();
        let mut armed = 0_usize;

        for _ in 0..CHURN_ROUNDS {
            let mut fixture = spawn_shell("exit 0", &[], None);
            let pid = fixture.pid();
            let identity = match inspector.identity(pid) {
                Ok(Some(identity)) => identity,
                Ok(None) | Err(Error::Race { .. }) => {
                    fixture.0.wait().expect("reap the fixture");
                    continue;
                }
                Err(error) => panic!("unexpected identity failure: {error}"),
            };
            match inspector.exit_watch(identity) {
                Ok(watch) => {
                    armed += 1;
                    await_exit("the churn exit", watch).await;
                    assert_ne!(
                        inspector.identity(pid).expect("reinspect identity"),
                        Some(identity),
                        "a completed watch means the exact identity is gone"
                    );
                }
                Err(Error::Race { .. }) => {}
                Err(error) => panic!("unexpected watch failure: {error}"),
            }
            fixture.0.wait().expect("reap the fixture");
        }

        assert!(armed > 0, "the churn fixture must arm at least one watch");
    }

    #[tokio::test]
    async fn many_idle_watches_stay_pending_and_then_all_complete() {
        let inspector = DarwinInspector::new();
        let mut fixture = spawn_shell("sleep 60", &[], None);
        let identity = live_identity(inspector, fixture.pid());

        let mut watches = Vec::with_capacity(IDLE_WATCH_COUNT);
        for _ in 0..IDLE_WATCH_COUNT {
            watches.push(inspector.exit_watch(identity).expect("arm an idle watch"));
        }
        let first = watches.pop().expect("at least one idle watch");
        assert!(
            tokio::time::timeout(IDLE_WATCH_WINDOW, first.wait())
                .await
                .is_err(),
            "a watch on a live process must stay pending"
        );

        fixture.0.kill().expect("kill the fixture");
        for watch in watches {
            await_exit("an idle watch", watch).await;
        }
    }

    #[tokio::test]
    async fn cancelled_watches_release_their_descriptors() {
        let inspector = DarwinInspector::new();
        let fixture = spawn_idle_shell();
        let identity = live_identity(inspector, fixture.pid());

        for _ in 0..IDLE_WATCH_COUNT {
            drop(inspector.exit_watch(identity).expect("arm a watch"));
        }

        inspector
            .exit_watch(identity)
            .expect("descriptors are released promptly enough to arm another watch");
    }

    #[test]
    fn foreground_group_tracks_the_terminal_owner() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::Read;

        let inspector = DarwinInspector::new();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pseudoterminal");
        let mut command = CommandBuilder::new("/bin/bash");
        command.args(["--norc", "--noprofile", "-i"]);
        command.env("PS1", "READY>");
        let child = pair.slave.spawn_command(command).expect("spawn the shell");
        let shell_pid = child.process_id().expect("shell process id");
        let shell = PtyFixture {
            child,
            pid: shell_pid,
        };
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("clone the terminal reader");
        let mut writer = pair.master.take_writer().expect("take the terminal writer");
        drop(pair.slave);

        let (prompt, prompt_seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut byte = [0_u8; 1];
            while reader.read(&mut byte).is_ok_and(|read| read == 1) {
                seen.push(byte[0]);
                if seen.ends_with(b"READY>") {
                    let _ = prompt.send(());
                    return;
                }
            }
        });
        prompt_seen
            .recv_timeout(OBSERVE_TIMEOUT)
            .expect("the interactive shell never printed its prompt");

        assert_eq!(
            observe("the idle shell foreground group", move || {
                inspector.foreground_process_group(shell_pid)
            })
            .expect("inspect the shell foreground group"),
            Some(shell_pid),
            "an idle job-control shell owns its own terminal"
        );

        std::io::Write::write_all(&mut writer, b"sleep 60 &\nsleep 60\n")
            .expect("start a background and a foreground job");
        std::io::Write::flush(&mut writer).expect("flush the terminal writer");

        let foreground = wait_for("the shell to hand the terminal to a job", || {
            observe("the foreground group of a running job", move || {
                inspector.foreground_process_group(shell_pid)
            })
            .expect("inspect the foreground group")
            .filter(|group| *group != shell_pid)
        });
        let root = wait_for("the shell to become observable", || {
            observe("the shell identity", move || inspector.identity(shell_pid))
                .expect("inspect the shell identity")
        });
        let members: Vec<_> = observe("the descendants of a running job", move || {
            inspector.descendants(root.pid)
        })
        .expect("inspect the shell descendants")
        .into_iter()
        .filter(|fact| fact.pgid == foreground)
        .collect();
        assert!(
            !members.is_empty(),
            "the foreground group must resolve to real members"
        );
        assert!(
            observe("the process groups of a running job", move || {
                inspector.descendants(root.pid)
            })
            .expect("inspect the shell descendants")
            .iter()
            .any(|fact| fact.pgid != foreground),
            "the background sibling keeps its own process group"
        );

        let member_pid = members.first().expect("a foreground group member").pid;
        // Signalling the observer's own process group would stop this test
        // process instead of the job, so the resolved group is checked first.
        let own_group = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get())
            .expect("a process group fits an unsigned process id");
        assert_ne!(
            foreground, own_group,
            "a shell terminal foreground group must never resolve to the observer's own group"
        );
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(foreground).expect("group fits a pid")),
            nix::sys::signal::Signal::SIGTSTP,
        )
        .expect("stop the foreground job");

        // The record and the argument region of a stopped process are read
        // through different kernel interfaces, so they are observed separately.
        assert!(
            observe("the identity of a stopped process", move || {
                inspector.identity(member_pid)
            })
            .expect("inspect a stopped process identity")
            .is_some(),
            "a stopped process keeps its identity while it holds its process id"
        );
        assert!(
            observe("the facts of a stopped process", move || {
                inspector.process(member_pid)
            })
            .expect("inspect a stopped process")
            .is_some(),
            "a stopped process stays observable while it holds its process id"
        );
        let reclaimed = wait_for("the shell to reclaim the terminal", || {
            observe("the foreground group of a stopped job", move || {
                inspector.foreground_process_group(shell_pid)
            })
            .expect("inspect the foreground group")
            .filter(|group| *group == shell_pid)
        });
        assert_eq!(reclaimed, shell_pid);
        assert!(
            observe("the descendants of a stopped job", move || {
                inspector.descendants(root.pid)
            })
            .expect("inspect the shell descendants")
            .iter()
            .any(|fact| fact.pgid == foreground),
            "a stopped foreground job stays observable without owning the terminal"
        );

        // A pseudoterminal shell does not always report its exit to the parent
        // that spawned it, so reaping is bounded and its outcome is evidence
        // rather than a requirement. What the inspector observes is the claim
        // that matters: the killed shell no longer runs under its identity.
        let reaped = observe("the killed shell to be reaped", move || {
            let mut shell = shell;
            shell.reap()
        });
        let identity = observe("the killed shell identity", move || {
            inspector.identity(shell_pid)
        });
        assert!(
            !observe("the killed shell liveness", move || {
                inspector.is_running(root)
            })
            .expect("inspect the killed shell"),
            "a killed shell must stop running (reaped: {reaped}, identity: {identity:?})"
        );
    }
}
