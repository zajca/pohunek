//! Reads kernel-derived Unix peer credentials on Linux.

use std::os::fd::AsFd;

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

use super::{Credentials, Error};

// Rust guideline compliant 2026-09-13

/// Reads credentials from an accepted connected Unix socket.
///
/// # Errors
///
/// Returns [`Error`] when the kernel lookup fails or the PID is invalid.
pub fn credentials(socket: &impl AsFd) -> Result<Credentials, Error> {
    let native =
        getsockopt(socket, PeerCredentials).map_err(|error| Error::Socket(error.into()))?;
    Ok(Credentials {
        uid: native.uid(),
        gid: native.gid(),
        pid: u32::try_from(native.pid()).map_err(|_range_error| Error::InvalidPid)?,
    })
}

#[cfg(test)]
mod tests {
    use super::credentials;
    use std::os::unix::net::UnixStream;

    #[test]
    fn peer_credentials_come_from_connected_socket() {
        let (client, server) = UnixStream::pair().expect("Unix socket pair");
        let client_credentials = credentials(&client).expect("client peer credentials");
        let server_credentials = credentials(&server).expect("server peer credentials");
        assert_eq!(client_credentials.pid, std::process::id());
        assert_eq!(server_credentials.pid, std::process::id());
        assert_eq!(client_credentials.uid, server_credentials.uid);
        assert_eq!(client_credentials.gid, server_credentials.gid);
    }
}
