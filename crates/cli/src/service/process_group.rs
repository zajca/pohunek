//! Whether a process group still has a live member.
//!
//! On Darwin `killpg(group, 0)` ends in `killpg1` (`bsd/kern/kern_sig.c`),
//! which counts the members it may signal and skips zombies (`p_stat ==
//! SZOMB`) and system processes. A group found without a counted member
//! answers `EPERM` for a POSIX caller, the same errno as a live member of
//! another user, so the answer alone does not separate a group that only
//! holds zombies from a live one. A membership query does. An emptied group
//! the kernel can no longer find answers `ESRCH`.
//!
//! Linux answers `ESRCH` for a group without members, and a zombie member
//! still counts as signalable, so `EPERM` there means a live member of
//! another user.

// Rust guideline compliant 2026-10-22

/// The result of a membership query.
#[derive(Debug)]
pub(super) struct Membership {
    /// Whether any member of the group is alive; a zombie is not.
    pub(super) live: bool,
    /// Each member with its state, for the failure message.
    pub(super) detail: String,
}

/// Lists the members of `group` and whether any of them is alive.
///
/// On Linux an `EPERM` probe already proves a live member, so this reports
/// one without a query.
///
/// # Errors
///
/// Returns the errno of the failed membership query, so the caller never
/// takes a terminal from a group it could not inspect.
pub(super) fn membership(group: nix::unistd::Pid) -> nix::Result<Membership> {
    imp::membership(group)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Membership;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "shares its signature with the Darwin query"
    )]
    pub(super) fn membership(_group: nix::unistd::Pid) -> nix::Result<Membership> {
        Ok(Membership {
            live: true,
            detail: "[not queried]".to_owned(),
        })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use nix::errno::Errno;

    use super::Membership;

    /// A member as `proc_pidinfo` reports it.
    #[derive(Debug, Clone, Copy)]
    enum State {
        Alive,
        Zombie,
        /// Gone between the listing and the query.
        Ended,
    }

    pub(super) fn membership(group: nix::unistd::Pid) -> nix::Result<Membership> {
        let mut live = false;
        let mut detail = Vec::new();
        for pid in members(group)? {
            let state = state_of(pid)?;
            live |= matches!(state, State::Alive);
            detail.push(format!("{pid}:{state:?}"));
        }
        Ok(Membership {
            live,
            detail: format!("[{}]", detail.join(",")),
        })
    }

    #[expect(unsafe_code, reason = "libproc is queried through the libc bindings")]
    fn state_of(pid: libc::pid_t) -> nix::Result<State> {
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
            .map_err(|_unrepresentable| Errno::EINVAL)?;
        // SAFETY: `info` is a plain-data buffer of the size passed.
        let (written, errno, info) = unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            Errno::clear();
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
            Ok(if info.pbi_status == libc::SZOMB {
                State::Zombie
            } else {
                State::Alive
            })
        } else if errno == Errno::ESRCH {
            Ok(State::Ended)
        } else if errno == Errno::UnknownErrno {
            Err(Errno::EIO)
        } else {
            Err(errno)
        }
    }

    /// The pids `proc_listpgrppids` reports for `group`, zombies included.
    ///
    /// The call returns a pid count, and 0 both for an empty group and for a
    /// failed query, which it reports only through `errno`. With a null
    /// buffer the count is the kernel's estimate for the whole system, not
    /// for the group, so the buffer is sized from it and the listing's own
    /// count says how many entries are valid.
    #[expect(unsafe_code, reason = "libproc is queried through the libc bindings")]
    fn members(group: nix::unistd::Pid) -> nix::Result<Vec<libc::pid_t>> {
        // SAFETY: a null buffer of size 0 only asks for the size estimate.
        let estimate = unsafe {
            Errno::clear();
            let estimate = libc::proc_listpgrppids(group.as_raw(), std::ptr::null_mut(), 0);
            checked(estimate)?
        };
        // Members may join between the estimate and the listing.
        let capacity = estimate + 8;
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

    /// The pid count of a `proc_listpgrppids` result: a negative count, or a
    /// zero with `errno` set, is a failed query.
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
    use super::Membership;

    pub(super) fn membership(_group: nix::unistd::Pid) -> nix::Result<Membership> {
        Err(nix::errno::Errno::ENOSYS)
    }
}
