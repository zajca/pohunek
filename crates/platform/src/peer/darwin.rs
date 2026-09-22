//! Reads kernel-derived Unix peer identity on Darwin.
//!
//! Darwin splits what Linux answers in one call. `LOCAL_PEERCRED` returns the
//! `xucred` the kernel copied from the connecting process inside `unp_connect`,
//! so its user and group are connection-time identity. `LOCAL_PEERPID` returns
//! the peer socket's `last_pid`, which the kernel re-stamps whenever a
//! different process operates on that socket, so the process id is
//! inspection-time identity and callers must re-read it before acting.
//!
//! The kernel fills the peer record only where it set `UNP_HAVEPC`: the
//! accepted side of a listener, and both ends of a `socketpair`. The connecting
//! side of a real `connect` answers `ENOTCONN` even while fully connected, so
//! that errno means "this socket carries no peer record", not "not connected".
//!
//! `LOCAL_PEERTOKEN` was evaluated and rejected. The kernel resolves it through
//! the same `last_pid`, so it does not detect a descriptor hand-off either;
//! `audit_token_t` is absent from `libc`, its field layout is undocumented, and
//! `libbsm` is deprecated. The shared process start identity already gives
//! process-reuse protection on both targets.

// Rust guideline compliant 2026-09-22

use std::os::fd::BorrowedFd;

use super::darwin_layout::{LayoutError, XuCred};
use super::{Credentials, Error};

// A transcription slip in the mirrored record must fail the build rather than
// decode kernel bytes at the wrong offsets.
const _: () = assert!(size_of::<XuCred>() == size_of::<libc::xucred>());

/// Reads credentials from an accepted connected Unix socket.
///
/// # Errors
///
/// Returns [`Error`] when the kernel carries no peer record for this socket,
/// answers with a malformed record, or reports no usable process identifier.
pub(super) fn credentials(socket: BorrowedFd<'_>) -> Result<Credentials, Error> {
    let owner = native::peer_owner(socket)?;
    let pid = native::peer_pid(socket)?;
    Ok(Credentials {
        uid: owner.uid,
        gid: owner.gid,
        pid,
    })
}

/// Maps a rejected kernel record onto the shared typed error.
fn layout_error(error: LayoutError) -> Error {
    match error {
        LayoutError::Truncated
        | LayoutError::UnsupportedVersion
        | LayoutError::InvalidGroupCount => Error::Malformed,
    }
}

/// Wraps the two Darwin peer socket options behind safe, checked functions.
///
/// Nothing outside this module touches a raw pointer or a `libc` type; callers
/// receive owned Rust values or the shared typed error.
#[expect(
    unsafe_code,
    reason = "LOCAL_PEERCRED and LOCAL_PEERPID have no safe Rust equivalent; each block documents its invariant"
)]
mod native {
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd};

    use super::super::darwin_layout::{decode, Owner, XuCred};
    use super::{layout_error, Error};

    /// Reads the connection-time owner record for one accepted socket.
    pub(super) fn peer_owner(socket: BorrowedFd<'_>) -> Result<Owner, Error> {
        let mut record = XuCred::zeroed();
        let mut length = option_length(size_of::<XuCred>())?;
        // SAFETY: `getsockopt` writes at most `length` bytes, and `record` is a
        // live, correctly aligned allocation of exactly that many bytes.
        let outcome = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                std::ptr::addr_of_mut!(record).cast::<libc::c_void>(),
                &raw mut length,
            )
        };
        if outcome != 0 {
            return Err(classify(io::Error::last_os_error()));
        }
        let written = usize::try_from(length).map_err(|_range| Error::Malformed)?;
        decode(&record, written).map_err(layout_error)
    }

    /// Reads the inspection-time process id of one accepted socket's peer.
    pub(super) fn peer_pid(socket: BorrowedFd<'_>) -> Result<u32, Error> {
        let mut pid: libc::c_int = 0;
        let mut length = option_length(size_of::<libc::c_int>())?;
        // SAFETY: `getsockopt` writes at most `length` bytes, and `pid` is a
        // live, correctly aligned allocation of exactly that many bytes.
        let outcome = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                std::ptr::addr_of_mut!(pid).cast::<libc::c_void>(),
                &raw mut length,
            )
        };
        if outcome != 0 {
            return Err(classify(io::Error::last_os_error()));
        }
        if usize::try_from(length) != Ok(size_of::<libc::c_int>()) {
            return Err(Error::Malformed);
        }
        if pid == 0 {
            // The kernel leaves `last_pid` zero for a socket it created itself.
            // There is no weaker identity to fall back to.
            return Err(Error::PidUnavailable);
        }
        u32::try_from(pid).map_err(|_range| Error::InvalidPid)
    }

    /// Converts a buffer width to the native option length.
    fn option_length(bytes: usize) -> Result<libc::socklen_t, Error> {
        libc::socklen_t::try_from(bytes).map_err(|_range| Error::Malformed)
    }

    /// Classifies a `getsockopt` failure without inventing a fallback identity.
    ///
    /// `ENOTCONN` here means the socket carries no peer record, which includes
    /// the connecting side of a live connection. Either way the caller gets an
    /// explicit rejection, never a downgrade to owner-only authentication.
    fn classify(source: io::Error) -> Error {
        match source.raw_os_error() {
            Some(libc::ENOTCONN | libc::EINVAL | libc::ENOPROTOOPT | libc::EOPNOTSUPP) => {
                Error::Unavailable
            }
            _ => Error::Socket(source),
        }
    }
}
