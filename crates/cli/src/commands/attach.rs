//! `pohunek attach` — attach the local terminal to a PTY-backed session.
//!
//! Works against a local or remote host: the control RPCs go through the
//! transport-agnostic [`Client`], and the raw second connection (the attach byte
//! stream) is opened over the *same* transport via [`crate::client::attach_raw`].
//! Press Ctrl-] (0x1d) while attached to detach from the session without
//! stopping the PTY process.

use std::os::fd::RawFd;
use std::path::Path;
use std::time::Duration;

use protocol::{
    event, method, Request, SessionAttachParams, SessionAttachResult, SessionDetachParams,
    SessionId, SessionResizeParams, ENV_DAEMON_ID, ENV_SESSION_ID,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

use crate::client::{attach_raw, Client, RawStream};
use crate::commands::{request_id, request_with_params};
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::Target;

const DETACH_BYTE: u8 = 0x1d;
const IO_BUFFER_BYTES: usize = 8192;
// Frequent enough to repair fullscreen TUIs that reset margins, while low enough
// to avoid turning idle attaches into a busy terminal repaint loop.
const DEFAULT_BANNER_REPAINT_INTERVAL: Duration = Duration::from_millis(500);
// Two rows are not enough for a banner plus a usable agent viewport.
const MIN_ROWS_WITH_BANNER: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachBannerConfig {
    enabled: bool,
    repaint_interval: Duration,
}

impl AttachBannerConfig {
    fn disabled() -> Self {
        Self {
            enabled: false,
            repaint_interval: DEFAULT_BANNER_REPAINT_INTERVAL,
        }
    }

    fn load_from_config_dir(config_dir: &Path) -> Result<Self, CliError> {
        let path = config_dir.join("launcher.conf");
        if !path.try_exists()? {
            return Ok(Self::disabled());
        }

        let contents = std::fs::read_to_string(&path)?;
        let mut config = Self::disabled();
        for (number, raw) in contents.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(config_error(format!(
                    "{}:{}: expected key=value",
                    path.display(),
                    number + 1
                )));
            };
            match key.trim() {
                "banner" => config.enabled = parse_bool(value.trim(), &path, number + 1)?,
                "banner_interval_seconds" => {
                    config.repaint_interval =
                        parse_duration_seconds(value.trim(), &path, number + 1)?;
                }
                _ => {}
            }
        }
        Ok(config)
    }

    fn load(paths: &Paths) -> Result<Self, CliError> {
        let config_dir = std::env::var("POHUNEK_CONFIG_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .map_or_else(|| paths.config_dir.clone(), std::path::PathBuf::from);
        Self::load_from_config_dir(&config_dir)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachBannerSnapshot {
    host: String,
    id: String,
    agent: String,
    state: String,
    activity: String,
}

#[derive(Debug)]
struct AttachBannerRuntime {
    snapshot: AttachBannerSnapshot,
    terminal_size: (u16, u16),
    repaint_interval: Duration,
}

impl AttachBannerSnapshot {
    fn unknown(host: &str, id: &str) -> Self {
        Self {
            host: host.to_owned(),
            id: id.to_owned(),
            agent: "<unknown>".to_owned(),
            state: "<unknown>".to_owned(),
            activity: "-".to_owned(),
        }
    }

    fn update_from_session_value(&mut self, value: &serde_json::Value) {
        if value.get("id").and_then(serde_json::Value::as_str) != Some(self.id.as_str()) {
            return;
        }
        if let Some(agent) = value.get("agent").and_then(serde_json::Value::as_str) {
            replace_string(&mut self.agent, agent);
        }
        if let Some(state) = value.get("state").and_then(serde_json::Value::as_str) {
            replace_string(&mut self.state, state);
        }
        if let Some(activity) = value.get("activity").and_then(serde_json::Value::as_str) {
            replace_string(&mut self.activity, activity);
        }
    }

    fn update_from_event_value(&mut self, value: &serde_json::Value) {
        match value.get("event").and_then(serde_json::Value::as_str) {
            Some(event::AGENT_STATE) => {
                if value.get("session_id").and_then(serde_json::Value::as_str)
                    != Some(self.id.as_str())
                {
                    return;
                }
                if let Some(activity) = value.get("activity").and_then(serde_json::Value::as_str) {
                    replace_string(&mut self.activity, activity);
                }
            }
            Some(event::SESSION_CREATED | event::SESSION_UPDATED | event::SESSION_STOPPED) => {
                if let Some(session) = value.get("session") {
                    self.update_from_session_value(session);
                }
            }
            _ => {}
        }
    }
}

fn replace_string(target: &mut String, value: &str) {
    target.clear();
    target.push_str(value);
}

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
    let banner_config = AttachBannerConfig::load(paths)?;
    let mut client = Client::connect(host, paths).await?;
    let result = client.request(&attach_request).await?;
    let attach: SessionAttachResult = serde_json::from_value(result)?;

    // Open the raw second connection over the same transport as the control
    // connection. The SDK writes the daemon attach prelude before returning, so
    // the CLI only owns terminal resize/forward/detach behavior.
    match attach_raw(host, paths, &attach.stream_id).await? {
        // Box the large attach future to keep this enclosing future small.
        RawStream::Local(stream) => {
            Box::pin(attach_over_stream(
                stream,
                client,
                &attach.stream_id,
                host,
                paths,
                target,
                banner_config,
            ))
            .await
        }
        RawStream::Remote(stream) => {
            Box::pin(attach_over_stream(
                stream,
                client,
                &attach.stream_id,
                host,
                paths,
                target,
                banner_config,
            ))
            .await
        }
        _ => unreachable!("unsupported raw attach stream transport"),
    }
}

/// Push an initial resize, then bridge the terminal and the stream until
/// detach/EOF — generic over the transport.
///
/// Mirrors the original local sequence after SDK attach negotiation: best-effort
/// resize on the control connection, then the forward loop.
async fn attach_over_stream<S>(
    stream: S,
    mut client: Client,
    stream_id: &str,
    host: &str,
    paths: &Paths,
    target: &Target,
    banner_config: AttachBannerConfig,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let terminal_size = terminal_size(libc::STDOUT_FILENO);
    let banner_terminal_size =
        terminal_size.filter(|size| effective_attach_size(*size, true).is_some());
    let banner_enabled = banner_config.enabled && banner_terminal_size.is_some();

    if let Some((cols, rows)) =
        terminal_size.and_then(|size| effective_attach_size(size, banner_enabled))
    {
        if let Ok(request) = build_resize_request(target, cols, rows) {
            let _ = client.request(&request).await;
        }
    }

    let (banner, banner_updates) = if let Some(terminal_size) =
        banner_terminal_size.filter(|_| banner_config.enabled)
    {
        let snapshot = AttachBannerSnapshot::unknown(&banner_host_label(host), &target.session_id);
        let updates = spawn_banner_updates(
            host.to_owned(),
            paths.clone(),
            target.session_id.clone(),
            snapshot.clone(),
        );
        (
            Some(AttachBannerRuntime {
                snapshot,
                terminal_size,
                repaint_interval: banner_config.repaint_interval,
            }),
            Some(updates),
        )
    } else {
        (None, None)
    };

    forward_attached_stream(
        stream,
        client,
        stream_id.to_owned(),
        target,
        banner,
        banner_updates,
    )
    .await
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

fn effective_attach_size((cols, rows): (u16, u16), banner_enabled: bool) -> Option<(u16, u16)> {
    if !banner_enabled {
        return Some((cols, rows));
    }
    if rows < MIN_ROWS_WITH_BANNER {
        None
    } else {
        Some((cols, rows - 1))
    }
}

fn render_banner_frame(cols: u16, rows: u16, snapshot: &AttachBannerSnapshot) -> String {
    let text = truncate_banner_text(
        &format!(
            "{}/{}  agent={}  state={}  activity={}",
            snapshot.host, snapshot.id, snapshot.agent, snapshot.state, snapshot.activity
        ),
        cols,
    );
    format!("\x1b[s\x1b[?6l\x1b[1;1H\x1b[7m\x1b[2K{text}\x1b[0m\x1b[2;{rows}r\x1b[?6h\x1b[u")
}

fn reset_banner_frame() -> &'static str {
    "\x1b[s\x1b[?6l\x1b[r\x1b[0m\x1b[1;1H\x1b[2K\x1b[u"
}

fn truncate_banner_text(text: &str, cols: u16) -> String {
    text.chars().take(usize::from(cols)).collect()
}

fn banner_host_label(host: &str) -> String {
    if host.is_empty() {
        "local".to_owned()
    } else {
        host.to_owned()
    }
}

fn spawn_banner_updates(
    host: String,
    paths: Paths,
    session_id: String,
    initial: AttachBannerSnapshot,
) -> mpsc::UnboundedReceiver<AttachBannerSnapshot> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut snapshot = initial;
        let Ok(mut client) = Client::connect(&host, &paths).await else {
            return;
        };

        if let Ok(request) =
            request_with_params(method::SESSION_INSPECT, &SessionId(session_id.clone()))
        {
            if let Ok(info) = client.request(&request).await {
                snapshot.update_from_session_value(&info);
                let _ = tx.send(snapshot.clone());
            }
        }

        let request = Request::new(
            request_id(method::SUBSCRIBE),
            method::SUBSCRIBE,
            serde_json::Value::Null,
        );
        let Ok(mut subscription) = client.into_sdk().subscribe(&request).await else {
            return;
        };
        while let Ok(Some(line)) = subscription.next_line().await {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let before = snapshot.clone();
            snapshot.update_from_event_value(&event);
            if snapshot != before {
                let _ = tx.send(snapshot.clone());
            }
        }
    });
    rx
}

async fn repaint_banner<W>(writer: &mut W, banner: &AttachBannerRuntime) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(
            render_banner_frame(
                banner.terminal_size.0,
                banner.terminal_size.1,
                &banner.snapshot,
            )
            .as_bytes(),
        )
        .await?;
    writer.flush().await?;
    Ok(())
}

async fn reset_banner<W>(writer: &mut W) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(reset_banner_frame().as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn repaint_banner_if_active<W>(
    writer: &mut W,
    banner: Option<&AttachBannerRuntime>,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(banner) = banner {
        repaint_banner(writer, banner).await?;
    }
    Ok(())
}

async fn reset_banner_if_active<W>(writer: &mut W, banner_enabled: bool) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if banner_enabled {
        reset_banner(writer).await?;
    }
    Ok(())
}

fn parse_bool(value: &str, path: &Path, number: usize) -> Result<bool, CliError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(config_error(format!(
            "{}:{number}: invalid boolean value {other:?}; expected true or false",
            path.display()
        ))),
    }
}

fn parse_duration_seconds(value: &str, path: &Path, number: usize) -> Result<Duration, CliError> {
    let seconds = value.parse::<f64>().map_err(|err| {
        config_error(format!(
            "{}:{number}: invalid duration value {value:?}: {err}",
            path.display()
        ))
    })?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(config_error(format!(
            "{}:{number}: banner_interval_seconds must be greater than zero",
            path.display()
        )));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn config_error(message: String) -> CliError {
    CliError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
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
    mut banner: Option<AttachBannerRuntime>,
    mut banner_updates: Option<mpsc::UnboundedReceiver<AttachBannerSnapshot>>,
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
    let mut repaint_tick = time::interval(
        banner
            .as_ref()
            .map_or(DEFAULT_BANNER_REPAINT_INTERVAL, |banner| {
                banner.repaint_interval
            }),
    );
    repaint_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    repaint_banner_if_active(&mut stdout, banner.as_ref()).await?;

    loop {
        tokio::select! {
            read = socket_read.read(&mut socket_buf) => {
                let bytes_read = read?;
                if bytes_read == 0 {
                    reset_banner_if_active(&mut stdout, banner.is_some()).await?;
                    return Ok(());
                }
                stdout.write_all(&socket_buf[..bytes_read]).await?;
                stdout.flush().await?;
                repaint_banner_if_active(&mut stdout, banner.as_ref()).await?;
            }
            read = stdin.read(&mut stdin_buf) => {
                let bytes_read = read?;
                if bytes_read == 0 {
                    socket_write.shutdown().await?;
                    reset_banner_if_active(&mut stdout, banner.is_some()).await?;
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
                    reset_banner_if_active(&mut stdout, banner.is_some()).await?;
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
                    if let Some(banner) = banner.as_mut() {
                        banner.terminal_size = (cols, rows);
                    }
                    let effective_size = effective_attach_size((cols, rows), banner.is_some())
                        .unwrap_or((cols, rows));
                    if let Ok(request) = build_resize_request(target, effective_size.0, effective_size.1) {
                        let _ = client.request(&request).await;
                    }
                    repaint_banner_if_active(&mut stdout, banner.as_ref()).await?;
                }
            }
            update = async {
                match banner_updates.as_mut() {
                    Some(updates) => updates.recv().await,
                    None => None,
                }
            }, if banner_updates.is_some() => {
                if let Some(snapshot) = update {
                    if let Some(banner) = banner.as_mut() {
                        banner.snapshot = snapshot;
                    }
                    repaint_banner_if_active(&mut stdout, banner.as_ref()).await?;
                } else {
                    banner_updates = None;
                }
            }
            _ = repaint_tick.tick(), if banner.is_some() => {
                repaint_banner_if_active(&mut stdout, banner.as_ref()).await?;
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

#[expect(
    unsafe_code,
    reason = "calls libc::isatty, the sole way to probe a tty"
)]
fn is_tty(fd: RawFd) -> bool {
    // SAFETY: `isatty` only reads the file descriptor value and does not require
    // any Rust-side aliasing or lifetime guarantees.
    unsafe { libc::isatty(fd) == 1 }
}

#[expect(unsafe_code, reason = "zero-initializes a plain C termios struct")]
fn zeroed_termios() -> libc::termios {
    // SAFETY: `termios` is a plain C data struct. It is immediately initialized
    // by `tcgetattr` before any field is read.
    unsafe { std::mem::zeroed() }
}

#[expect(
    unsafe_code,
    reason = "calls libc::tcgetattr to read terminal attributes"
)]
fn tcgetattr(fd: RawFd, termios: &mut libc::termios) -> Result<(), CliError> {
    // SAFETY: `termios` points to valid writable memory for the duration of the
    // call, and `fd` is checked by libc.
    if unsafe { libc::tcgetattr(fd, termios) } == -1 {
        Err(CliError::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[expect(
    unsafe_code,
    reason = "calls libc::tcsetattr to apply terminal attributes"
)]
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

#[expect(
    unsafe_code,
    reason = "zero-inits winsize and calls libc::ioctl(TIOCGWINSZ)"
)]
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
    use serde_json::json;

    use super::*;

    #[expect(
        clippy::needless_pass_by_value,
        reason = "test helper takes the json! literal by value to keep call sites terse"
    )]
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

    #[test]
    fn attach_banner_config_reads_launcher_conf_from_override_dir() {
        let root = std::env::temp_dir().join(format!(
            "pohunek-attach-banner-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create config dir");
        std::fs::write(
            root.join("launcher.conf"),
            "banner=true\nbanner_interval_seconds=0.25\n",
        )
        .expect("write config");

        let config = AttachBannerConfig::load_from_config_dir(&root).expect("load config");

        assert!(config.enabled, "banner=true should enable attach overlay");
        assert_eq!(
            config.repaint_interval,
            std::time::Duration::from_millis(250)
        );
    }

    #[test]
    fn attach_banner_reserves_one_terminal_row_for_daemon_resize() {
        assert_eq!(effective_attach_size((120, 40), true), Some((120, 39)));
        assert_eq!(effective_attach_size((120, 40), false), Some((120, 40)));
        assert_eq!(effective_attach_size((120, 2), true), None);
    }

    #[test]
    fn attach_banner_frame_draws_top_row_and_restores_session_region() {
        let frame = render_banner_frame(
            80,
            24,
            &AttachBannerSnapshot {
                host: "local".to_owned(),
                id: "s-42".to_owned(),
                agent: "claude".to_owned(),
                state: "running".to_owned(),
                activity: "blocked".to_owned(),
            },
        );

        assert!(
            frame.starts_with("\x1b[s\x1b[?6l\x1b[1;1H\x1b[7m\x1b[2K"),
            "banner frame must save cursor and draw on physical row one: {frame:?}"
        );
        assert!(
            frame.contains("local/s-42  agent=claude  state=running  activity=blocked"),
            "banner text should include session state: {frame:?}"
        );
        assert!(
            frame.ends_with("\x1b[0m\x1b[2;24r\x1b[?6h\x1b[u"),
            "banner frame must restore the scroll region below the banner: {frame:?}"
        );
    }

    #[test]
    fn attach_banner_snapshot_updates_from_inspect_and_event_payloads() {
        let mut snapshot = AttachBannerSnapshot::unknown("local", "s-42");

        snapshot.update_from_session_value(&json!({
            "id": "s-42",
            "agent": "claude",
            "state": "running",
            "activity": "working"
        }));
        assert_eq!(snapshot.agent, "claude");
        assert_eq!(snapshot.state, "running");
        assert_eq!(snapshot.activity, "working");

        snapshot.update_from_event_value(&json!({
            "event": "agent_state",
            "session_id": "s-42",
            "activity": "blocked"
        }));
        assert_eq!(snapshot.activity, "blocked");

        snapshot.update_from_event_value(&json!({
            "event": "session_updated",
            "session": {
                "id": "s-42",
                "agent": "codex",
                "state": "done"
            }
        }));
        assert_eq!(snapshot.agent, "codex");
        assert_eq!(snapshot.state, "done");
        assert_eq!(snapshot.activity, "blocked");

        snapshot.update_from_event_value(&json!({
            "event": "agent_state",
            "session_id": "s-99",
            "activity": "idle"
        }));
        assert_eq!(snapshot.activity, "blocked");
    }
}
