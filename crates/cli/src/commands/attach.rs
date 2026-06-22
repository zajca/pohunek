//! `pohunek attach` — attach the local terminal to a PTY-backed session.
//!
//! Works against a local or remote host: the control RPCs go through the
//! transport-agnostic [`Client`], and the raw second connection (the attach byte
//! stream) is opened over the *same* transport via [`crate::client::connect_raw`].
//! Press Ctrl-] (0x1d) while attached to detach from the session without
//! stopping the PTY process.

use std::os::fd::RawFd;

use protocol::{
    method, AttachHeader, Request, SessionAttachParams, SessionAttachResult, SessionDetachParams,
    SessionId, SessionResizeParams, ENV_DAEMON_ID, ENV_SESSION_ID,
};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};

use crate::client::{connect_raw, Client, RawStream};
use crate::commands::request_id;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::Target;

const DETACH_BYTE: u8 = 0x1d;
const IO_BUFFER_BYTES: usize = 8192;

/// Run top-level `attach` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, attach negotiation fails, terminal raw mode cannot be configured, or
/// raw I/O fails.
pub(crate) async fn run_attach(host: &str, paths: &Paths, target: &Target) -> Result<(), CliError> {
    // Tell the daemon which session+instance this client is itself running inside,
    // so it can refuse a self-feeding attach loop before it starts. Reported for
    // every transport: the loop is reachable even over a same-host loopback TCP
    // attach, and the daemon-id pairing prevents a false positive against a
    // different daemon that reuses the same session-id string.
    let (origin_session_id, origin_daemon_id) = self_feedback_origin();
    let attach_request = build_attach_request(target, origin_session_id, origin_daemon_id)?;
    let mut client = Client::connect(host, paths).await?;
    let result = client.request(&attach_request).await?;
    let attach: SessionAttachResult = serde_json::from_value(result)?;

    // Open the raw second connection over the same transport as the control
    // connection, so the attach byte stream rides the local socket or the remote
    // NetBird TCP connection consistently. Dispatch on the transport once, then
    // run the identical (generic) header -> resize -> forward sequence in each arm.
    match connect_raw(host, paths).await? {
        RawStream::Local(stream) => {
            attach_over_stream(stream, client, &attach.stream_id, target).await
        }
        RawStream::Remote(stream) => {
            attach_over_stream(stream, client, &attach.stream_id, target).await
        }
    }
}

/// Send the attach header, push an initial resize, then bridge the terminal and
/// the stream until detach/EOF — generic over the transport.
///
/// Mirrors the original local sequence exactly: header first, then a best-effort
/// resize on the control connection, then the forward loop.
async fn attach_over_stream<S>(
    mut stream: S,
    mut client: Client,
    stream_id: &str,
    target: &Target,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send_attach_header(&mut stream, stream_id).await?;

    if let Some((cols, rows)) = terminal_size(libc::STDOUT_FILENO) {
        if let Ok(request) = build_resize_request(target, cols, rows) {
            let _ = client.request(&request).await;
        }
    }

    forward_attached_stream(stream, client, stream_id.to_owned(), target).await
}

fn request_with_params<T>(method: &str, params: &T) -> Result<Request, CliError>
where
    T: Serialize + ?Sized,
{
    Ok(Request::new(
        request_id(method),
        method,
        serde_json::to_value(params)?,
    ))
}

// Host routing is the transport's job; the request carries only the session id
// (identical on either side), never the host. The `origin_*` pair is the
// session+daemon this client runs inside; reporting it lets the daemon reject a
// self-feeding attach (see [`self_feedback_origin`]).
fn build_attach_request(
    target: &Target,
    origin_session_id: Option<SessionId>,
    origin_daemon_id: Option<String>,
) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_ATTACH,
        &SessionAttachParams {
            session_id: SessionId(target.session_id.clone()),
            origin_session_id,
            origin_daemon_id,
        },
    )
}

/// The (session, daemon instance) this attach is being launched from, when
/// reporting it lets the daemon refuse a self-feeding loop (attaching to a
/// session from inside its own terminal pipes its output back into its input).
///
/// Read from `POHUNEK_SESSION_ID` / `POHUNEK_DAEMON_ID`, which the daemon injects
/// into every session PTY, so both are present exactly when this process runs
/// inside a session's PTY. Reported regardless of the target host: the daemon
/// compares them against its OWN live instance, so a remote/loopback attach to a
/// *different* daemon (which reuses the same id string) is not falsely rejected,
/// while a same-host loopback to this daemon is correctly caught.
fn self_feedback_origin() -> (Option<SessionId>, Option<String>) {
    self_feedback_origin_from(
        std::env::var(ENV_SESSION_ID).ok(),
        std::env::var(ENV_DAEMON_ID).ok(),
    )
}

/// Pure core of [`self_feedback_origin`], split out so empty-value filtering is
/// unit-testable without touching the process env.
fn self_feedback_origin_from(
    raw_session_id: Option<String>,
    raw_daemon_id: Option<String>,
) -> (Option<SessionId>, Option<String>) {
    let session_id = raw_session_id.filter(|id| !id.is_empty()).map(SessionId);
    let daemon_id = raw_daemon_id.filter(|id| !id.is_empty());
    (session_id, daemon_id)
}

fn build_detach_request(stream_id: &str) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_DETACH,
        &SessionDetachParams {
            stream_id: stream_id.to_owned(),
        },
    )
}

fn build_resize_request(target: &Target, cols: u16, rows: u16) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_RESIZE,
        &SessionResizeParams {
            session_id: SessionId(target.session_id.clone()),
            cols,
            rows,
        },
    )
}

/// Write the attach header line over any byte stream.
///
/// Generic over the transport so the local Unix socket and the remote NetBird
/// TCP connection share one implementation.
async fn send_attach_header<S>(stream: &mut S, stream_id: &str) -> Result<(), CliError>
where
    S: AsyncWrite + Unpin,
{
    let mut header = serde_json::to_vec(&AttachHeader {
        attach: stream_id.to_owned(),
    })?;
    header.push(b'\n');
    stream.write_all(&header).await?;
    stream.flush().await?;
    Ok(())
}

/// Bidirectionally bridge the terminal and the attach byte stream until detach
/// or EOF, over any transport.
///
/// Uses [`tokio::io::split`] (generic) rather than a transport-specific
/// `into_split`, so the loop is identical for the local socket and a remote TCP
/// connection.
async fn forward_attached_stream<S>(
    stream: S,
    mut client: Client,
    stream_id: String,
    target: &Target,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let terminal = RawTerminal::enable(libc::STDIN_FILENO)?;
    let (mut socket_read, mut socket_write) = tokio::io::split(stream);
    let mut winch = signal(SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buf = [0_u8; IO_BUFFER_BYTES];
    let mut socket_buf = [0_u8; IO_BUFFER_BYTES];

    loop {
        tokio::select! {
            read = socket_read.read(&mut socket_buf) => {
                let bytes_read = read?;
                if bytes_read == 0 {
                    return Ok(());
                }
                stdout.write_all(&socket_buf[..bytes_read]).await?;
                stdout.flush().await?;
            }
            read = stdin.read(&mut stdin_buf) => {
                let bytes_read = read?;
                if bytes_read == 0 {
                    socket_write.shutdown().await?;
                    return Ok(());
                }

                if let Some(detach_at) = stdin_buf[..bytes_read]
                    .iter()
                    .position(|byte| *byte == DETACH_BYTE)
                {
                    if detach_at > 0 {
                        socket_write.write_all(&stdin_buf[..detach_at]).await?;
                        socket_write.flush().await?;
                    }
                    drop(terminal);
                    let _ = send_detach(&mut client, &stream_id).await;
                    return Ok(());
                }

                socket_write.write_all(&stdin_buf[..bytes_read]).await?;
                socket_write.flush().await?;
            }
            resized = winch.recv() => {
                if resized.is_none() {
                    continue;
                }
                if let Some((cols, rows)) = terminal_size(libc::STDOUT_FILENO) {
                    if let Ok(request) = build_resize_request(target, cols, rows) {
                        let _ = client.request(&request).await;
                    }
                }
            }
        }
    }
}

async fn send_detach(client: &mut Client, stream_id: &str) -> Result<(), CliError> {
    let request = build_detach_request(stream_id)?;
    let _ = client.request(&request).await?;
    Ok(())
}

#[derive(Debug)]
struct RawTerminal {
    fd: RawFd,
    original: libc::termios,
}

impl RawTerminal {
    fn enable(fd: RawFd) -> Result<Option<Self>, CliError> {
        if !is_tty(fd) {
            return Ok(None);
        }

        let mut original = zeroed_termios();
        tcgetattr(fd, &mut original)?;
        let mut raw = original;
        make_raw(&mut raw);
        tcsetattr(fd, &raw)?;

        Ok(Some(Self { fd, original }))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, &self.original);
    }
}

#[allow(unsafe_code)]
fn is_tty(fd: RawFd) -> bool {
    // SAFETY: `isatty` only reads the file descriptor value and does not require
    // any Rust-side aliasing or lifetime guarantees.
    unsafe { libc::isatty(fd) == 1 }
}

#[allow(unsafe_code)]
fn zeroed_termios() -> libc::termios {
    // SAFETY: `termios` is a plain C data struct. It is immediately initialized
    // by `tcgetattr` before any field is read.
    unsafe { std::mem::zeroed() }
}

#[allow(unsafe_code)]
fn tcgetattr(fd: RawFd, termios: &mut libc::termios) -> Result<(), CliError> {
    // SAFETY: `termios` points to valid writable memory for the duration of the
    // call, and `fd` is checked by libc.
    if unsafe { libc::tcgetattr(fd, termios) } == -1 {
        Err(CliError::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[allow(unsafe_code)]
fn tcsetattr(fd: RawFd, termios: &libc::termios) -> Result<(), CliError> {
    // SAFETY: `termios` points to valid initialized memory for the duration of
    // the call, and `fd` is checked by libc.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) } == -1 {
        Err(CliError::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn make_raw(termios: &mut libc::termios) {
    termios.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
    termios.c_oflag &= !libc::OPOST;
    termios.c_cflag |= libc::CS8;
    termios.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;
}

#[allow(unsafe_code)]
fn terminal_size(fd: RawFd) -> Option<(u16, u16)> {
    if !is_tty(fd) {
        return None;
    }

    // SAFETY: `winsize` is a plain C data struct filled by `ioctl` before its
    // fields are read.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `size` points to valid writable memory for the duration of the
    // call, and `fd` is checked by libc.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == -1 {
        return None;
    }

    if size.ws_col == 0 || size.ws_row == 0 {
        None
    } else {
        Some((size.ws_col, size.ws_row))
    }
}

#[cfg(test)]
mod tests {
    use protocol::{method, Request};
    use serde_json::json;

    use super::*;
    use crate::target::Target;

    fn assert_request(request: &Request, method_name: &str, params: serde_json::Value) {
        assert_eq!(request.v.get(), 1, "envelope version");
        assert_eq!(request.method, method_name, "method");
        assert_eq!(request.params, params, "params");
        // The id is now a unique per-call correlation id; assert only its
        // stable, log-greppable `cli-<method>-` prefix.
        assert!(
            request.id.starts_with(&format!("cli-{method_name}-")),
            "id {:?} must be prefixed by the method",
            request.id
        );
    }

    #[test]
    fn attach_request_sends_session_id() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_attach_request(&target, None, None).expect("request");

        assert_request(
            &request,
            method::SESSION_ATTACH,
            json!({
                "session_id": "s-42"
            }),
        );
    }

    #[test]
    fn attach_request_includes_origin_when_present() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_attach_request(
            &target,
            Some(SessionId("s-42".to_owned())),
            Some("daemon-xyz".to_owned()),
        )
        .expect("request");

        assert_request(
            &request,
            method::SESSION_ATTACH,
            json!({
                "session_id": "s-42",
                "origin_session_id": "s-42",
                "origin_daemon_id": "daemon-xyz"
            }),
        );
    }

    #[test]
    fn self_feedback_origin_reports_session_and_daemon_when_present() {
        // Inside a session's PTY: report both so the daemon can pin the loop.
        assert_eq!(
            self_feedback_origin_from(Some("s-7".to_owned()), Some("daemon-1".to_owned())),
            (
                Some(SessionId("s-7".to_owned())),
                Some("daemon-1".to_owned())
            )
        );
        // Not inside any session (env unset or empty): nothing to report.
        assert_eq!(self_feedback_origin_from(None, None), (None, None));
        assert_eq!(
            self_feedback_origin_from(Some(String::new()), Some(String::new())),
            (None, None)
        );
        // A session id without a daemon id cannot be pinned to an instance; it is
        // still forwarded, and the daemon declines to reject without a daemon id.
        assert_eq!(
            self_feedback_origin_from(Some("s-7".to_owned()), None),
            (Some(SessionId("s-7".to_owned())), None)
        );
    }

    #[test]
    fn detach_request_sends_stream_id() {
        let request = build_detach_request("stream-42").expect("request");

        assert_request(
            &request,
            method::SESSION_DETACH,
            json!({
                "stream_id": "stream-42"
            }),
        );
    }

    #[test]
    fn resize_request_sends_local_session_id_and_size() {
        let target: Target = "s-42".parse().expect("target");
        let request = build_resize_request(&target, 120, 40).expect("request");

        assert_request(
            &request,
            method::SESSION_RESIZE,
            json!({
                "session_id": "s-42",
                "cols": 120,
                "rows": 40
            }),
        );
    }

    #[test]
    fn attach_request_extracts_session_id_regardless_of_host() {
        // Remote is now supported: the attach request carries only the session
        // id; the host selects the transport, it never enters the request body.
        let remote: Target = "host-b/s-42".parse().expect("target");
        let request = build_attach_request(&remote, None, None).expect("request");

        assert_request(
            &request,
            method::SESSION_ATTACH,
            json!({
                "session_id": "s-42"
            }),
        );
    }

    #[test]
    fn resize_request_extracts_session_id_regardless_of_host() {
        let remote: Target = "host-b/s-42".parse().expect("target");
        let request = build_resize_request(&remote, 120, 40).expect("request");

        assert_request(
            &request,
            method::SESSION_RESIZE,
            json!({
                "session_id": "s-42",
                "cols": 120,
                "rows": 40
            }),
        );
    }
}
