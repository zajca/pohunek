//! Whether a process group still has a live member.
//!
//! Darwin answers `killpg(group, 0)` with `EPERM` both for a group that
//! exists without a signalable member (the terminal keeps the object of its
//! foreground group after the last member left) and for a live group of
//! another user, so a membership query separates the two. Linux answers
//! `ESRCH` for an empty group and `EPERM` only while a member exists that
//! the caller may not signal.

// Rust guideline compliant 2026-10-22

/// Whether any process of `group` is alive; a zombie is not.
///
/// On Linux an `EPERM` probe already proves a live member, so this reports
/// one without a query.
///
/// # Errors
///
/// Returns the errno of the failed membership query, so the caller never
/// takes a terminal from a group it could not inspect.
pub(super) fn has_live_member(group: nix::unistd::Pid) -> nix::Result<bool> {
    imp::has_live_member(group)
}

#[cfg(target_os = "linux")]
mod imp {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "shares its signature with the Darwin query"
    )]
    pub(super) fn has_live_member(_group: nix::unistd::Pid) -> nix::Result<bool> {
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use nix::errno::Errno;

    pub(super) fn has_live_member(group: nix::unistd::Pid) -> nix::Result<bool> {
        for pid in members(group)? {
            if is_alive(pid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `pid` is a process that has not ended; a zombie has.
    #[expect(unsafe_code, reason = "libproc is queried through the libc bindings")]
    fn is_alive(pid: libc::pid_t) -> nix::Result<bool> {
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
            .map_err(|_unrepresentable| Errno::EINVAL)?;
        // SAFETY: `info` is a plain-data buffer of the size passed.
        let (written, errno, info) = unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let written = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::from_mut(&mut info).cast(),
                size,
            );
            (written, Errno::last(), info)
        };
        if written == size {
            Ok(info.pbi_status != libc::SZOMB)
        } else if errno == Errno::ESRCH {
            // The process ended since the listing.
            Ok(false)
        } else {
            Err(errno)
        }
    }

    /// The pids `proc_listpgrppids` reports for `group`, zombies included.
    ///
    /// The call returns a pid count, and 0 both for an empty group and for a
    /// failed query, which it reports only through `errno`.
    #[expect(unsafe_code, reason = "libproc is queried through the libc bindings")]
    fn members(group: nix::unistd::Pid) -> nix::Result<Vec<libc::pid_t>> {
        // SAFETY: a null buffer of size 0 only asks for the pid count.
        let needed = unsafe {
            Errno::clear();
            let needed = libc::proc_listpgrppids(group.as_raw(), std::ptr::null_mut(), 0);
            checked(needed)?
        };
        // Members may join between the size query and the listing.
        let capacity = needed + 8;
        let mut pids: Vec<libc::pid_t> = vec![0; capacity];
        let bytes = i32::try_from(capacity * std::mem::size_of::<libc::pid_t>())
            .map_err(|_unrepresentable| Errno::EINVAL)?;
        // SAFETY: `pids` owns `bytes` writable bytes.
        let written = unsafe {
            Errno::clear();
            let written = libc::proc_listpgrppids(group.as_raw(), pids.as_mut_ptr().cast(), bytes);
            checked(written)?
        };
        pids.truncate(written);
        pids.retain(|pid| *pid > 0);
        Ok(pids)
    }

    /// The pid count of a `proc_listpgrppids` result: a negative count or a
    /// zero with `errno` set is a failed query.
    fn checked(count: i32) -> nix::Result<usize> {
        match (usize::try_from(count), Errno::last()) {
            (Ok(0), errno) if errno != Errno::UnknownErrno => Err(errno),
            (Ok(count), _) => Ok(count),
            (Err(_negative), errno) => Err(errno),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    pub(super) fn has_live_member(_group: nix::unistd::Pid) -> nix::Result<bool> {
        Err(nix::errno::Errno::ENOSYS)
    }
}
