//! Defines kernel-derived Unix peer identity for owner-private sockets.
//!
//! Peer identity has two parts with different guarantees, and callers must not
//! confuse them.
//!
//! **Connection-time identity** is the peer's effective user and group. Both
//! kernels freeze it when the connection is established: Linux stores it in the
//! socket's `SO_PEERCRED` record, and Darwin copies the connecting process's
//! credentials into the accepted socket inside `unp_connect`. It cannot drift.
//!
//! **Inspection-time identity** is the peer process id. Linux freezes it with
//! the rest of `SO_PEERCRED`, but Darwin serves `LOCAL_PEERPID` from the peer
//! socket's `last_pid`, which the kernel re-stamps whenever a *different*
//! process operates on that socket. A descriptor inherited across `fork`/`exec`
//! or passed over `SCM_RIGHTS` therefore changes the reported peer on Darwin.
//!
//! [`Binding`] exists for that difference: it keeps a duplicated descriptor so a
//! service can re-read the kernel answer before each authority decision and
//! reject a peer that changed. On Linux the re-read is stable by construction,
//! so one call expresses the same contract on both targets.
//!
//! Nothing here interprets the values. Authorization policy stays with the
//! accepting service.

// Rust guideline compliant 2026-09-26

use std::os::fd::{AsFd, OwnedFd};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
use linux as backend;

// The pure `xucred` decoding is built on every host so its size, version, and
// group-count checks are covered by the ordinary workspace test gate instead of
// only by the macOS runner.
#[cfg(all(unix, any(target_os = "macos", test)))]
mod darwin_layout;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
use darwin as backend;

/// Kernel-derived identity of one connected Unix peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    /// Effective user identifier reported by the kernel.
    pub uid: u32,
    /// Effective group identifier reported by the kernel.
    pub gid: u32,
    /// Connecting process identifier reported by the kernel.
    pub pid: u32,
}

/// Kernel interface that produced one peer credential snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Linux `SO_PEERCRED`, frozen by the kernel at connect time.
    PeerCred,
    /// Darwin `LOCAL_PEERCRED` for the owner plus `LOCAL_PEERPID` for the id.
    LocalPeerCred,
}

impl Provenance {
    /// Returns whether the kernel freezes the peer id at connect time.
    ///
    /// Darwin answers `false`: its peer id tracks the last process to operate
    /// on the peer socket, so a caller that acts on it must re-verify.
    #[must_use]
    pub const fn pid_is_connect_time(self) -> bool {
        matches!(self, Self::PeerCred)
    }
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::PeerCred => "SO_PEERCRED",
            Self::LocalPeerCred => "LOCAL_PEERCRED",
        };
        f.write_str(name)
    }
}

/// Returns the kernel interface this target reads peer identity from.
#[must_use]
pub const fn provenance() -> Provenance {
    #[cfg(target_os = "linux")]
    {
        Provenance::PeerCred
    }
    #[cfg(target_os = "macos")]
    {
        Provenance::LocalPeerCred
    }
}

/// Reads credentials from one accepted, connected Unix socket.
///
/// # Errors
///
/// Returns [`Error`] when the kernel cannot attest the socket, answers with a
/// malformed record, or reports no usable process identifier.
pub fn credentials(socket: &impl AsFd) -> Result<Credentials, Error> {
    backend::credentials(socket.as_fd())
}

/// Reads the serving process's credentials from a connecting Unix socket.
///
/// A client uses this to learn which process serves a socket path. Both
/// kernels attest the connecting side with the server's identity. Linux
/// answers from the `SO_PEERCRED` record the listener stamped when it called
/// `listen`. Darwin answers `LOCAL_PEERCRED` from the listener's cached
/// credentials and `LOCAL_PEERPID` from the server-side socket's `last_pid`,
/// which names the process that last operated on that socket and stays zero
/// until one did.
///
/// Call this after the server has answered on the connection and while it is
/// still open: only then do both kernels name the process that served it, and
/// Darwin cannot read a peer id from a closed connection at all.
///
/// # Errors
///
/// Returns [`Error`] when the socket is not connected, the kernel cannot
/// attest it, or it reports no usable process identifier.
pub fn server(socket: &impl AsFd) -> Result<Credentials, Error> {
    backend::credentials(socket.as_fd())
}

/// Live binding to one accepted Unix peer.
///
/// The binding owns a duplicate of the accepted descriptor, so
/// [`Binding::revalidate`] keeps working after the connection has been split
/// into read and write halves. That costs one descriptor per private
/// connection, which is the price of detecting a Darwin descriptor hand-off;
/// restructuring every caller to retain the original socket was rejected as far
/// more invasive for the same guarantee. The duplicate is close-on-exec, so a
/// process the service spawns never inherits it.
#[derive(Debug)]
pub struct Binding {
    socket: OwnedFd,
    snapshot: Credentials,
}

impl Binding {
    /// Captures peer credentials from an accepted socket.
    ///
    /// Call this on the socket returned by `accept`, before reading any request
    /// byte, so the snapshot describes the process that connected. Only an
    /// accepted socket names the right process: a listening socket answers with
    /// the *listener's own* credentials, an unconnected Linux socket answers
    /// with a zero process id, and the connecting side of a Darwin connection
    /// answers with the listener's credentials rather than refusing. None of
    /// those is an error, which is exactly why the caller has to pick the right
    /// socket.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the descriptor cannot be duplicated or the kernel
    /// cannot attest the peer.
    pub fn capture(socket: &impl AsFd) -> Result<Self, Error> {
        let socket = socket.as_fd().try_clone_to_owned().map_err(Error::Socket)?;
        let snapshot = backend::credentials(socket.as_fd())?;
        Ok(Self { socket, snapshot })
    }

    /// Returns the credentials captured when the connection was accepted.
    #[must_use]
    pub const fn snapshot(&self) -> Credentials {
        self.snapshot
    }

    /// Re-reads kernel credentials and requires them to match the capture.
    ///
    /// This detects a *hand-off* — the socket now answering for a different
    /// process — not process-identifier *reuse*. A caller that needs reuse
    /// protection pairs the peer id with the process start identity from
    /// [`crate::process`], which is what makes the pair unambiguous within one
    /// boot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Changed`] when the kernel now reports a different peer,
    /// or the underlying lookup failure.
    pub fn revalidate(&self) -> Result<(), Error> {
        let current = backend::credentials(self.socket.as_fd())?;
        if current == self.snapshot {
            Ok(())
        } else {
            Err(Error::Changed)
        }
    }
}

/// Peer-credential lookup failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The kernel rejected credential lookup.
    #[error("failed to read Unix peer credentials: {0}")]
    Socket(std::io::Error),
    /// The kernel cannot attest this socket at all.
    #[error("Unix peer credentials are unavailable on this socket")]
    Unavailable,
    /// The kernel returned a process identifier outside the shared range.
    #[error("Unix peer PID is outside the supported range")]
    InvalidPid,
    /// The kernel reported no peer process identifier.
    #[error("the kernel reported no Unix peer PID")]
    PidUnavailable,
    /// The kernel answer failed a size, version, or group-count check.
    #[error("Unix peer credentials from the kernel are malformed")]
    Malformed,
    /// The kernel now reports a different peer than the captured one.
    #[error("Unix peer identity changed after the connection was captured")]
    Changed,
}

#[cfg(test)]
mod tests {
    use super::{credentials, provenance, server, Binding, Error};
    use std::os::unix::net::{UnixListener, UnixStream};

    /// A client names the process serving a socket path from its own side.
    #[test]
    fn connecting_side_names_the_serving_process() {
        use std::io::{Read as _, Write as _};

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("served.sock");
        let listening = UnixListener::bind(&path).expect("bind served socket");
        let mut client = UnixStream::connect(&path).expect("connect served socket");
        let (mut accepted, _address) = listening.accept().expect("accept served socket");
        // The server answers first, as a daemon does before the client asks.
        accepted.write_all(b"\n").expect("server answers");
        let mut answer = [0_u8; 1];
        client
            .read_exact(&mut answer)
            .expect("client reads the answer");

        let served_by = server(&client).expect("server credentials");
        assert_eq!(served_by.pid, std::process::id());
        assert_eq!(
            served_by.uid,
            credentials(&accepted).expect("peer credentials").uid
        );
    }

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

    #[test]
    fn binding_captures_the_connecting_process_and_stays_stable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("peer.sock");
        let listener = UnixListener::bind(&path).expect("bind peer socket");
        let client = UnixStream::connect(&path).expect("connect peer socket");
        let (accepted, _address) = listener.accept().expect("accept peer socket");

        let binding = Binding::capture(&accepted).expect("capture peer");
        assert_eq!(binding.snapshot().pid, std::process::id());
        binding.revalidate().expect("peer identity is unchanged");

        // What a closed peer means is the one place the two kernels disagree,
        // and both answers reject rather than silently pass.
        drop(client);
        let after_close = binding.revalidate();
        if cfg!(target_os = "macos") {
            // `LOCAL_PEERPID` needs a live connection, so the peer id stops
            // being readable once the other side drops.
            assert!(
                matches!(after_close, Err(Error::Unavailable)),
                "a dropped peer must stop being attestable, got {after_close:?}"
            );
        } else {
            // `SO_PEERCRED` answers from a record the kernel keeps, so the
            // socket still names the same peer; liveness is a process question.
            after_close.expect("credentials survive peer close");
        }
    }

    #[test]
    fn host_provenance_states_whether_the_peer_id_is_frozen() {
        let expected = cfg!(target_os = "linux");
        assert_eq!(provenance().pid_is_connect_time(), expected);
    }

    /// A socket the kernel cannot attribute to a peer process must be rejected
    /// explicitly rather than answered with a weaker identity.
    #[cfg(target_os = "linux")]
    #[test]
    fn socket_without_a_peer_process_is_rejected() {
        use nix::sys::socket::{socket, AddressFamily, SockFlag, SockType};

        // `SO_PEERCRED` succeeds here and fills pid 0 with overflow uid/gid, so
        // the zero check is the only thing standing between the caller and a
        // fabricated identity.
        let unconnected = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .expect("unconnected Unix socket");
        let error = credentials(&unconnected).expect_err("socket has no peer process");
        assert!(
            matches!(error, Error::PidUnavailable),
            "expected an explicit missing-PID rejection, got {error:?}"
        );
    }

    /// Linux answers for a listening socket with the listener's own identity,
    /// which is why a caller must only ever capture an accepted socket.
    #[cfg(target_os = "linux")]
    #[test]
    fn listening_socket_reports_the_listener_itself() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("listener.sock");
        let listener = UnixListener::bind(&path).expect("bind listener");
        let reported = credentials(&listener).expect("listener credentials");
        assert_eq!(reported.pid, std::process::id());
    }

    /// Darwin attests the connecting side too, with the *listener's* identity.
    ///
    /// `unp_connect` copies the listener's cached credentials into the
    /// connecting socket, so this side answers successfully while naming a
    /// process that never connected. That is the concrete reason
    /// [`Binding::capture`] is documented as accept-only: a caller cannot rely
    /// on the wrong socket failing to tell them apart.
    #[cfg(target_os = "macos")]
    #[test]
    fn connecting_side_is_attested_too() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("peer.sock");
        let listener = UnixListener::bind(&path).expect("bind peer socket");
        let client = UnixStream::connect(&path).expect("connect peer socket");
        let (accepted, _address) = listener.accept().expect("accept peer socket");

        let from_client = credentials(&client).expect("connecting side is attested");
        let from_accepted = credentials(&accepted).expect("accepted side is attested");
        assert_eq!(from_client.pid, std::process::id());
        assert_eq!(from_accepted.pid, std::process::id());
    }
}
