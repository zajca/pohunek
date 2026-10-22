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
/// a connection whose keepalive timing was not applied (the kernel then keeps
/// its own defaults) still works, and refusing it would turn a socket-option
/// error into an outage.
pub(super) fn apply(stream: &TcpStream, peer: SocketAddr, timing: KeepaliveTiming) {
    match set_and_read_back(stream, timing) {
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
            "remote keepalive setup failed; serving the connection with the kernel keepalive defaults"
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

fn set_and_read_back(stream: &TcpStream, timing: KeepaliveTiming) -> io::Result<Applied> {
    let socket = SockRef::from(stream);
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(timing.idle)
            .with_interval(timing.interval)
            .with_retries(timing.count),
    )?;
    Ok(Applied {
        enabled: socket.keepalive()?,
        idle_s: socket.tcp_keepalive_time()?.as_secs(),
        interval_s: socket.tcp_keepalive_interval()?.as_secs(),
        count: socket.tcp_keepalive_retries()?,
    })
}
