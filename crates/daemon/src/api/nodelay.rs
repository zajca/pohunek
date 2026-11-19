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

/// Sets `TCP_NODELAY` on `stream` and logs the result.
///
/// A failure is logged with the peer and the OS error and otherwise ignored:
/// a connection without `TCP_NODELAY` still works, and refusing it would turn a
/// socket-option error into an outage.
pub(super) fn apply(stream: &TcpStream, peer: SocketAddr) {
    if let Err(err) = SockRef::from(stream).set_tcp_nodelay(true) {
        warn!(
            peer = %peer,
            error = %err,
            errno = err.raw_os_error(),
            "remote TCP_NODELAY setup failed; small writes may be coalesced by Nagle"
        );
        return;
    }
    debug!(peer = %peer, nodelay.enabled = true, "remote TCP_NODELAY applied");
}
