//! TCP keepalive for accepted overlay control connections.
//!
//! A client host that vanishes without closing its connection sends nothing,
//! so a long wait it started would hold its waiter slot until the wait's own
//! deadline. Keepalive probes make the kernel fail the idle connection with a
//! read error, which the hangup watch in `dispatch_until_hangup` already turns
//! into a cancelled wait and a released slot.

use std::io;
use std::net::SocketAddr;

use pohunek_service_config::KeepaliveTiming;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// Sets `timing` on `stream` and logs the values the kernel reports back.
///
/// A failure is logged with the peer and the OS error and otherwise ignored:
/// a connection whose keepalive timing was not applied still works, and
/// refusing it would turn a socket-option error into an outage. The log says
/// which step failed, because the socket state differs: after a set error the
/// keepalive may be off or only partly configured, after a read-back error the
/// configured values are applied but could not be confirmed.
pub(super) fn apply(stream: &TcpStream, peer: SocketAddr, timing: KeepaliveTiming) {
    let socket = SockRef::from(stream);
    if let Err(err) = set(&socket, timing) {
        warn!(
            peer = %peer,
            error = %err,
            errno = err.raw_os_error(),
            "remote keepalive setup failed; keepalive may be off or only partly configured"
        );
        return;
    }
    match read_back(&socket) {
        Ok(applied) => debug!(
            peer = %peer,
            keepalive.enabled = applied.enabled,
            keepalive.idle_s = applied.idle_s,
            keepalive.interval_s = applied.interval_s,
            keepalive.count = applied.count,
            "remote keepalive applied"
        ),
        Err(err) => warn!(
            peer = %peer,
            error = %err,
            errno = err.raw_os_error(),
            "remote keepalive was set but reading the applied values back failed; the configured values are in effect"
        ),
    }
}

/// The option values the kernel reports for an accepted socket.
struct Applied {
    enabled: bool,
    idle_s: u64,
    interval_s: u64,
    count: u32,
}

fn set(socket: &SockRef<'_>, timing: KeepaliveTiming) -> io::Result<()> {
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(timing.idle)
            .with_interval(timing.interval)
            .with_retries(timing.count),
    )
}

fn read_back(socket: &SockRef<'_>) -> io::Result<Applied> {
    Ok(Applied {
        enabled: socket.keepalive()?,
        idle_s: socket.tcp_keepalive_time()?.as_secs(),
        interval_s: socket.tcp_keepalive_interval()?.as_secs(),
        count: socket.tcp_keepalive_retries()?,
    })
}
