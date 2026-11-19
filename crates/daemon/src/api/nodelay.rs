//! `TCP_NODELAY` for accepted overlay control connections.
//!
//! A remote client writes small frames (keystrokes, wheel reports) and reads
//! small replies. With Nagle on the daemon's side, a small write waits for the
//! ACK of the previous unacknowledged segment, so replies leave in RTT-spaced
//! bursts on any real link. Disabling Nagle on each accepted stream keeps the
//! daemon's direction uncoalesced; a socket-option failure never drops the
//! connection.

use std::net::SocketAddr;

use socket2::SockRef;
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// Sets `TCP_NODELAY` on `stream` and logs the value the kernel reports back.
///
/// A failure is logged with the peer and the OS error and otherwise ignored:
/// a connection without `TCP_NODELAY` still works, and refusing it would turn
/// a socket-option error into an outage. The log says which step failed,
/// because the socket state differs: after a set error Nagle may still be on,
/// after a read-back error the configured value is applied but could not be
/// confirmed.
pub(super) fn apply(stream: &TcpStream, peer: SocketAddr) {
    let socket = SockRef::from(stream);
    if let Err(err) = socket.set_tcp_nodelay(true) {
        warn!(
            peer = %peer,
            error = %err,
            errno = err.raw_os_error(),
            "remote TCP_NODELAY setup failed; small writes may be coalesced by Nagle"
        );
        return;
    }
    match socket.tcp_nodelay() {
        Ok(enabled) => debug!(
            peer = %peer,
            nodelay.enabled = enabled,
            "remote TCP_NODELAY applied"
        ),
        Err(err) => warn!(
            peer = %peer,
            error = %err,
            errno = err.raw_os_error(),
            "remote TCP_NODELAY was set but reading the applied value back failed; the configured value is in effect"
        ),
    }
}
