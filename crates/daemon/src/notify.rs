//! Minimal systemd readiness notification.

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

// Rust guideline compliant 2026-07-23

const NOTIFY_SOCKET_ENV: &str = "NOTIFY_SOCKET";
const READY_MESSAGE: &[u8] = b"READY=1";

/// Notifies systemd that daemon startup reconciliation is complete.
///
/// A missing `NOTIFY_SOCKET` means the daemon was started manually and is a
/// successful no-op.
///
/// # Errors
///
/// Returns an I/O error when systemd supplied a socket that cannot be reached.
pub fn ready() -> io::Result<()> {
    let Some(address) = std::env::var_os(NOTIFY_SOCKET_ENV) else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    let bytes = address.as_encoded_bytes();

    #[cfg(target_os = "linux")]
    if let Some(name) = bytes.strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt;

        let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
        socket.send_to_addr(READY_MESSAGE, &address)?;
        return Ok(());
    }

    socket.send_to(READY_MESSAGE, Path::new(&address))?;
    Ok(())
}
