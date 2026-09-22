//! Reads kernel-derived Unix peer credentials on Linux.
//!
//! `SO_PEERCRED` answers from a record the kernel wrote when the connection was
//! established, so every field here is connection-time identity and none of it
//! can drift while the socket stays open.
//!
//! The lookup stays on `nix` deliberately. `rustix::net::sockopt::socket_peercred`
//! returns a `Pid` wrapping `NonZeroI32` that it fills straight from the kernel
//! bytes, and the kernel legitimately answers zero here — for a socket that was
//! never connected, and for a peer living in a process-id namespace that is not
//! an ancestor of ours. That would be an invalid niche value rather than an
//! error. `nix` exposes the plain `ucred` fields, so a zero is observable and
//! becomes [`Error::PidUnavailable`].

// Rust guideline compliant 2026-09-22

use std::os::fd::BorrowedFd;

use nix::errno::Errno;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

use super::{Credentials, Error};

/// Reads credentials from an accepted connected Unix socket.
///
/// # Errors
///
/// Returns [`Error`] when the kernel cannot attest the socket, reports no peer
/// process, or reports a PID outside the shared range.
pub(super) fn credentials(socket: BorrowedFd<'_>) -> Result<Credentials, Error> {
    let native = getsockopt(&socket, PeerCredentials).map_err(classify)?;
    let pid = native.pid();
    if pid == 0 {
        // The kernel fills zero for a socket it cannot attribute to a visible
        // process. There is no weaker identity to fall back to.
        return Err(Error::PidUnavailable);
    }
    Ok(Credentials {
        uid: native.uid(),
        gid: native.gid(),
        pid: u32::try_from(pid).map_err(|_range| Error::InvalidPid)?,
    })
}

/// Classifies a `getsockopt` failure without inventing a fallback identity.
///
/// A socket the kernel refuses to attest is an explicit rejection, never a
/// downgrade to owner-only authentication.
fn classify(errno: Errno) -> Error {
    match errno {
        Errno::ENOTCONN | Errno::EINVAL | Errno::ENOPROTOOPT | Errno::EOPNOTSUPP => {
            Error::Unavailable
        }
        other => Error::Socket(other.into()),
    }
}
