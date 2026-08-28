//! `pohunek attach` — attach the local terminal to a PTY-backed session.
//!
//! Works against a local or remote host: the control RPCs go through the
//! transport-agnostic [`Client`], and the raw second connection (the attach byte
//! stream) is opened over the *same* transport via [`crate::client::attach_raw`].
//! Press Ctrl-] (0x1d) while attached to detach from the session without
//! stopping the PTY process.

mod shortcut;

use std::os::fd::RawFd;
use std::path::Path;
use std::time::Duration;

use pohunek_terminal::{step, Compositor, MenuEffect, MenuEvent, MenuKey, MenuOutcome, MenuState};
use protocol::{
    event, method, ForkCwdMode, Request, SessionAttachParams, SessionAttachResult,
    SessionDetachParams, SessionDetachResult, SessionForkParams, SessionForkResult, SessionId,
    SessionInfo, SessionNewParams, SessionNewResult, SessionRenameParams, SessionRenameResult,
    SessionResizeParams, SessionState, TerminalDimensions, ENV_DAEMON_ID, ENV_SESSION_ID,
    ENV_WORKER_ID,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time;

use self::shortcut::{MenuInputMode, Shortcut, ShortcutDecoder};
use crate::client::{Client, RawStream};
use crate::commands::{request_id, request_with_params};
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::Target;

const IO_BUFFER_BYTES: usize = 8192;
/// Maximum ambiguity delay for a split terminal shortcut sequence.
///
/// Physical stdin is local, so bytes from one key report normally arrive in a
/// single read. Twenty-five milliseconds still accommodates a split CSI-u
/// report without making a standalone Escape key perceptibly sluggish.
const SHORTCUT_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(25);
/// Selective terminal reset emitted whenever raw attach passthrough ends.
///
/// Agent TUIs may enable these DEC modes through ordinary PTY output. Termios
/// restoration does not disable them, so an unexpected stream close would make
/// the parent shell receive mouse, focus, or paste reports as text. This reset
/// returns to the main screen and normal shell-oriented modes without using the
/// destructive full-terminal RIS sequence.
const ATTACH_TERMINAL_MODE_CLEANUP: &[u8] = concat!(
    "\x1b[?1000l", // X10 mouse tracking.
    "\x1b[?1001l", // Mouse highlight tracking.
    "\x1b[?1002l", // Button-event mouse tracking.
    "\x1b[?1003l", // Any-event mouse tracking.
    "\x1b[?1004l", // Focus reporting.
    "\x1b[?1005l", // UTF-8 mouse coordinates.
    "\x1b[?1006l", // SGR mouse coordinates.
    "\x1b[?1015l", // urxvt mouse coordinates.
    "\x1b[?1016l", // SGR pixel mouse coordinates.
    "\x1b[?2004l", // Bracketed paste.
    "\x1b[?2026l", // Synchronized terminal updates.
    "\x1b[?1049l", // Alternate screen with saved cursor.
    "\x1b[?1047l", // Alternate screen fallback.
    "\x1b[?47l",   // Legacy alternate screen fallback.
    "\x1b[?1l",    // Normal cursor keys.
    "\x1b>",       // Normal numeric keypad.
    "\x1b[?6l",    // Absolute cursor addressing.
    "\x1b[?7h",    // Normal automatic line wrapping.
    "\x1b[r",      // Full-height scroll region.
    "\x1b[0m",     // Default character attributes.
    "\x1b[?25h",   // Visible cursor.
)
.as_bytes();
/// Maximum agent output retained while the attach menu is open.
///
/// Four MiB covers sustained repaint bursts while preventing an unattended
/// modal from growing memory without bound. Reaching the cap closes the modal
/// and resumes raw passthrough without dropping bytes.
const MAX_MODAL_BUFFER_BYTES: usize = 4 * 1024 * 1024;
// Two rows are not enough for a banner plus a usable agent viewport.
const MIN_ROWS_WITH_BANNER: u16 = 3;
const BANNER_MENU_LABEL: &str = "[menu:Ctrl-\\]";
// Two spaces keep dense banner fields readable in monospace terminals.
const BANNER_FIELD_SEPARATOR: &str = "  ";
/// Starting diagnostic generation for menu RPCs.
///
/// Result delivery is governed by the single in-flight task slot and current
/// modal state; the generation is retained only for log correlation.
const FIRST_MENU_GENERATION: u64 = 0;
/// Diagnostic generation increment for each explicit menu open.
const MENU_GENERATION_STEP: u64 = 1;
/// ASCII Escape starts keyboard CSI sequences and mouse reports.
const MENU_ESC_BYTE: u8 = 0x1b;
/// CSI sequences handled by the attach menu use `ESC [`.
const MENU_CSI_OPEN: u8 = b'[';
/// SGR mouse reports use `ESC [ < ...`.
const MENU_SGR_MOUSE_MARKER: u8 = b'<';
/// Legacy mouse reports use `ESC [ M ...`.
const MENU_LEGACY_MOUSE_MARKER: u8 = b'M';
/// Up-arrow CSI final byte.
const MENU_ARROW_UP: u8 = b'A';
/// Down-arrow CSI final byte.
const MENU_ARROW_DOWN: u8 = b'B';
/// SGR mouse press, wheel, and motion reports end with uppercase `M`.
const MENU_SGR_EVENT_FINAL: u8 = b'M';
/// SGR mouse release reports end with lowercase `m`.
const MENU_SGR_RELEASE_FINAL: u8 = b'm';
/// `ESC [` has two bytes before the final or marker byte.
const MENU_CSI_PREFIX_LEN: usize = 2;
/// Arrow-key reports handled here are `ESC [ A` and `ESC [ B`.
const MENU_ARROW_REPORT_LEN: usize = 3;
/// Legacy mouse reports are exactly `ESC [ M Cb Cx Cy`.
const MENU_LEGACY_MOUSE_REPORT_LEN: usize = 6;
/// Carriage return is the Enter byte in raw mode on many terminals.
const MENU_ENTER_CR: u8 = b'\r';
/// Line feed is accepted as Enter for testability and terminal variance.
const MENU_ENTER_LF: u8 = b'\n';
/// Delete-left byte emitted by common terminals for Backspace.
const MENU_BACKSPACE_DEL: u8 = 0x7f;
/// Ctrl-H is another common delete-left byte.
const MENU_BACKSPACE_CTRL_H: u8 = 0x08;
/// Default grace window for reattaching after a daemon restart.
///
/// Long enough for systemd to restart `pohunekd`, reconcile the durable session
/// worker, and restore the attach route, but short enough that a lost runtime
/// does not leave a dead attach terminal hanging indefinitely. Operators can
/// override it in `launcher.conf`; zero disables reconnect.
const DEFAULT_ATTACH_RECONNECT_WINDOW: Duration = Duration::from_secs(20);
/// Base delay for attach retry backoff and runtime-readiness polling.
///
/// A half-second cadence keeps reconnect responsive without turning a restart
/// outage into a tight socket-dial loop. Operators can override it in
/// `launcher.conf`.
const DEFAULT_ATTACH_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum automatic attach attempts after one unexpected stream closure.
///
/// Three attempts cover a normal daemon replacement without letting a
/// deterministic stream failure reopen control and subscription sockets
/// indefinitely. Operators can override the cap in `launcher.conf`.
const DEFAULT_ATTACH_RECONNECT_MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachReconnectConfig {
    window: Duration,
    interval: Duration,
    max_attempts: usize,
}

impl Default for AttachReconnectConfig {
    fn default() -> Self {
        Self {
            window: DEFAULT_ATTACH_RECONNECT_WINDOW,
            interval: DEFAULT_ATTACH_RECONNECT_INTERVAL,
            max_attempts: DEFAULT_ATTACH_RECONNECT_MAX_ATTEMPTS,
        }
    }
}

#[derive(Debug, Default)]
struct AttachReconnectBudget {
    deadline: Option<time::Instant>,
    attempts: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachReconnectPermit {
    Retry {
        deadline: time::Instant,
        attempt: usize,
    },
    Disabled,
    WindowExpired,
    AttemptsExhausted,
}

impl AttachReconnectBudget {
    fn next(
        &mut self,
        config: &AttachReconnectConfig,
        now: time::Instant,
    ) -> AttachReconnectPermit {
        if config.window.is_zero() {
            return AttachReconnectPermit::Disabled;
        }
        let deadline = *self.deadline.get_or_insert_with(|| now + config.window);
        if now >= deadline {
            return AttachReconnectPermit::WindowExpired;
        }
        if self.attempts >= config.max_attempts {
            return AttachReconnectPermit::AttemptsExhausted;
        }
        self.attempts += 1;
        AttachReconnectPermit::Retry {
            deadline,
            attempt: self.attempts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachConfig {
    reconnect: AttachReconnectConfig,
}

impl AttachConfig {
    fn defaults() -> Self {
        Self {
            reconnect: AttachReconnectConfig::default(),
        }
    }

    fn load_from_config_dir(config_dir: &Path) -> Result<Self, CliError> {
        let path = config_dir.join("launcher.conf");
        if !path.try_exists()? {
            return Ok(Self::defaults());
        }

        let contents = std::fs::read_to_string(&path)?;
        let mut config = Self::defaults();
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
                "attach_reconnect_seconds" => {
                    config.reconnect.window = parse_nonnegative_duration_seconds(
                        "attach_reconnect_seconds",
                        value.trim(),
                        &path,
                        number + 1,
                    )?;
                }
                "attach_reconnect_interval_seconds" => {
                    config.reconnect.interval = parse_positive_duration_seconds(
                        "attach_reconnect_interval_seconds",
                        value.trim(),
                        &path,
                        number + 1,
                    )?;
                }
                "attach_reconnect_max_attempts" => {
                    config.reconnect.max_attempts = parse_positive_usize(
                        "attach_reconnect_max_attempts",
                        value.trim(),
                        &path,
                        number + 1,
                    )?;
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
struct AttachStatusSnapshot {
    host: String,
    id: String,
    name: String,
    project: String,
    agent: String,
    state: String,
    activity: String,
}

/// Transient attach-modal state driving the [`Compositor`].
///
/// Agent bytes are forwarded directly while `active` is false. While the modal
/// is active they are buffered until the frozen physical screen is restored.
#[derive(Debug)]
struct ModalState {
    compositor: Compositor,
    snapshot: AttachStatusSnapshot,
    cols: u16,
    pending_output: Vec<u8>,
    active: bool,
}

#[derive(Debug)]
struct MenuInputDecoder {
    pending: Vec<u8>,
}

#[derive(Debug)]
struct MenuInputOutcome {
    effects: Vec<MenuEffect>,
    changed: bool,
}

#[derive(Debug)]
struct MenuTask {
    generation: u64,
    action: MenuTaskAction,
    handle: JoinHandle<Result<MenuOutcome, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuTaskAction {
    Kill,
    NewSession,
    Fork,
    Rename,
}

impl MenuTaskAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Kill => "kill",
            Self::NewSession => "new_session",
            Self::Fork => "fork",
            Self::Rename => "rename",
        }
    }
}

#[derive(Debug)]
struct MenuTaskResult {
    generation: u64,
    action: MenuTaskAction,
    outcome: Result<MenuOutcome, String>,
}

#[derive(Debug)]
struct MenuRuntime {
    state: Option<MenuState>,
    decoder: MenuInputDecoder,
    generation: u64,
    task: Option<MenuTask>,
}

#[derive(Debug, Clone)]
struct AttachControlContext {
    host: String,
    paths: Paths,
    target: Target,
}

#[derive(Debug)]
struct BannerUpdates {
    receiver: mpsc::UnboundedReceiver<AttachStatusSnapshot>,
    cancel: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct MenuEffectContext<'a> {
    client: &'a mut Client,
    stream_id: &'a str,
    terminal: &'a mut Option<RawTerminal>,
    menu_task: &'a mut Option<MenuTask>,
    generation: u64,
    control: &'a AttachControlContext,
    modal: Option<&'a ModalState>,
}

#[derive(Debug)]
struct StdinInputContext<'a, W, O> {
    socket_write: &'a mut W,
    stdout: &'a mut O,
    client: &'a mut Client,
    stream_id: &'a str,
    terminal: &'a mut Option<RawTerminal>,
    modal: &'a mut Option<ModalState>,
    menu: &'a mut MenuRuntime,
    shortcuts: &'a mut ShortcutDecoder,
    shortcut_deadline: &'a mut Option<time::Instant>,
    control: &'a AttachControlContext,
}

impl<W, O> StdinInputContext<'_, W, O> {
    fn reborrow(&mut self) -> StdinInputContext<'_, W, O> {
        StdinInputContext {
            socket_write: self.socket_write,
            stdout: self.stdout,
            client: self.client,
            stream_id: self.stream_id,
            terminal: self.terminal,
            modal: self.modal,
            menu: self.menu,
            shortcuts: self.shortcuts,
            shortcut_deadline: self.shortcut_deadline,
            control: self.control,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdinEvent {
    Input(usize),
    ShortcutTimeout,
}

impl AttachStatusSnapshot {
    fn unknown(host: &str, id: &str) -> Self {
        Self {
            host: host.to_owned(),
            id: id.to_owned(),
            name: id.to_owned(),
            project: "-".to_owned(),
            agent: "<unknown>".to_owned(),
            state: "<unknown>".to_owned(),
            activity: "-".to_owned(),
        }
    }

    fn update_from_session_value(&mut self, value: &serde_json::Value) {
        if value.get("id").and_then(serde_json::Value::as_str) != Some(self.id.as_str()) {
            return;
        }
        replace_string(
            &mut self.name,
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(self.id.as_str()),
        );
        replace_string(
            &mut self.project,
            value
                .get("project_label")
                .or_else(|| value.get("project_id"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-"),
        );
        if value.get("agent").is_some() || value.get("active_agent").is_some() {
            let agent = banner_agent_label(value, &self.agent);
            replace_string(&mut self.agent, &agent);
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

fn banner_agent_label(value: &serde_json::Value, current: &str) -> String {
    let launch = value
        .get("agent")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| {
            current
                .split_once("->")
                .map_or(current, |(launch, _)| launch)
        });
    match value
        .get("active_agent")
        .and_then(serde_json::Value::as_str)
    {
        Some(active) if active != launch => format!("{launch}->{active}"),
        _ => launch.to_owned(),
    }
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
    let (origin_session_id, origin_daemon_id, origin_worker_id) = self_feedback_origin();
    let attach_config = AttachConfig::load(paths)?;
    let mut reconnect = AttachReconnectBudget::default();

    loop {
        let end = run_attach_once(
            host,
            paths,
            target,
            origin_session_id.clone(),
            origin_daemon_id.clone(),
            origin_worker_id.clone(),
        )
        .await?;
        if end != AttachStreamEnd::StreamClosed {
            return Ok(());
        }
        match reconnect.next(&attach_config.reconnect, time::Instant::now()) {
            AttachReconnectPermit::Retry { deadline, attempt } => {
                if !wait_for_attach_reconnect(
                    host,
                    paths,
                    target,
                    &attach_config.reconnect,
                    deadline,
                    attempt,
                )
                .await?
                {
                    return Ok(());
                }
            }
            AttachReconnectPermit::Disabled => return Ok(()),
            AttachReconnectPermit::WindowExpired => {
                eprintln!(
                    "[pohunek] attach reconnect window expired for session {}",
                    target.session_id
                );
                return Ok(());
            }
            AttachReconnectPermit::AttemptsExhausted => {
                eprintln!(
                    "[pohunek] attach stream closed after {} automatic reattach attempts for \
                     session {}; giving up",
                    attach_config.reconnect.max_attempts, target.session_id
                );
                return Ok(());
            }
        }
    }
}

async fn run_attach_once(
    host: &str,
    paths: &Paths,
    target: &Target,
    origin_session_id: Option<SessionId>,
    origin_daemon_id: Option<String>,
    origin_worker_id: Option<String>,
) -> Result<AttachStreamEnd, CliError> {
    let initial_dimensions = current_terminal_dimensions();
    let attach_request = build_attach_request(
        target,
        initial_dimensions,
        origin_session_id,
        origin_daemon_id,
        origin_worker_id,
    )?;
    let mut client = Client::connect(host, paths).await?;
    let result = client.request(&attach_request).await?;
    let attach: SessionAttachResult = serde_json::from_value(result)?;

    // Open the raw second connection over the same transport as the control
    // connection. The SDK writes the daemon attach prelude before returning, so
    // the CLI only owns terminal resize/forward/detach behavior.
    match client.attach_raw(&attach.stream_id).await? {
        // Box the large attach future to keep this enclosing future small.
        RawStream::Local(stream) => {
            Box::pin(attach_over_stream(
                stream,
                client,
                &attach.stream_id,
                host,
                paths,
                target,
                initial_dimensions,
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
                initial_dimensions,
            ))
            .await
        }
        _ => unreachable!("unsupported raw attach stream transport"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachStreamEnd {
    Detached,
    InputClosed,
    SessionStopped,
    StreamClosed,
}

/// Bridges the terminal and stream after attach negotiation.
async fn attach_over_stream<S>(
    stream: S,
    mut client: Client,
    stream_id: &str,
    host: &str,
    paths: &Paths,
    target: &Target,
    initial_dimensions: Option<TerminalDimensions>,
) -> Result<AttachStreamEnd, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (modal, status_updates) = if let Some((cols, rows)) =
        terminal_size(libc::STDOUT_FILENO).filter(|&(_, rows)| rows >= MIN_ROWS_WITH_BANNER)
    {
        let snapshot = load_initial_banner_snapshot(&mut client, host, &target.session_id).await;
        // Best-effort live status updates keep the transient modal current. A
        // dropped subscription leaves the last snapshot available without
        // interrupting raw terminal passthrough.
        let updates = spawn_banner_updates(
            host.to_owned(),
            paths.clone(),
            target.session_id.clone(),
            snapshot.clone(),
        );
        let state = ModalState {
            compositor: Compositor::new(cols, rows),
            snapshot,
            cols,
            pending_output: Vec::new(),
            active: false,
        };
        (Some(state), Some(updates))
    } else {
        (None, None)
    };

    forward_attached_stream(
        stream,
        client,
        stream_id.to_owned(),
        AttachControlContext {
            host: host.to_owned(),
            paths: paths.clone(),
            target: target.clone(),
        },
        modal,
        status_updates,
        initial_dimensions,
    )
    .await
}

async fn load_initial_banner_snapshot(
    client: &mut Client,
    host: &str,
    session_id: &str,
) -> AttachStatusSnapshot {
    let mut snapshot = AttachStatusSnapshot::unknown(&banner_host_label(host), session_id);
    let Ok(request) =
        request_with_params(method::SESSION_INSPECT, &SessionId(session_id.to_owned()))
    else {
        return snapshot;
    };
    let Ok(info) = client.request(&request).await else {
        return snapshot;
    };
    snapshot.update_from_session_value(&info);
    snapshot
}

// Host routing is the transport's job; the request carries only the session id
// (identical on either side), never the host. The `origin_*` pair is the
// session+daemon this client runs inside; reporting it lets the daemon reject a
// self-feeding attach (see [`self_feedback_origin`]).
fn build_attach_request(
    target: &Target,
    initial_dimensions: Option<TerminalDimensions>,
    origin_session_id: Option<SessionId>,
    origin_daemon_id: Option<String>,
    origin_worker_id: Option<String>,
) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_ATTACH,
        &SessionAttachParams {
            session_id: SessionId(target.session_id.clone()),
            initial_dimensions,
            origin_session_id,
            origin_daemon_id,
            origin_worker_id,
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
fn self_feedback_origin() -> (Option<SessionId>, Option<String>, Option<String>) {
    self_feedback_origin_from(
        std::env::var(ENV_SESSION_ID).ok(),
        std::env::var(ENV_DAEMON_ID).ok(),
        std::env::var(ENV_WORKER_ID).ok(),
    )
}

/// Pure core of [`self_feedback_origin`], split out so empty-value filtering is
/// unit-testable without touching the process env.
fn self_feedback_origin_from(
    raw_session_id: Option<String>,
    raw_daemon_id: Option<String>,
    raw_worker_id: Option<String>,
) -> (Option<SessionId>, Option<String>, Option<String>) {
    let session_id = raw_session_id.filter(|id| !id.is_empty()).map(SessionId);
    let daemon_id = raw_daemon_id.filter(|id| !id.is_empty());
    let worker_id = raw_worker_id.filter(|id| !id.is_empty());
    (session_id, daemon_id, worker_id)
}

fn build_detach_request(stream_id: &str) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_DETACH,
        &SessionDetachParams {
            stream_id: stream_id.to_owned(),
        },
    )
}

fn build_inspect_request(target: &Target) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_INSPECT,
        &SessionId(target.session_id.clone()),
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

fn build_menu_new_session_request(
    _target: &Target,
    source: &SessionInfo,
    cols: u16,
    rows: u16,
) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_NEW,
        &SessionNewParams {
            agent: source.agent.clone(),
            name: None,
            cwd: Some(source.cwd.clone()),
            cols,
            rows,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: std::collections::BTreeMap::new(),
        },
    )
}

fn build_menu_rename_request(target: &Target, name: &str) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_RENAME,
        &SessionRenameParams {
            session_id: SessionId(target.session_id.clone()),
            name: Some(name.to_owned()),
        },
    )
}

fn build_menu_fork_request(target: &Target, cols: u16, rows: u16) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_FORK,
        &SessionForkParams {
            session_id: SessionId(target.session_id.clone()),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols,
            rows,
        },
    )
}

fn render_banner_text(cols: u16, snapshot: &AttachStatusSnapshot) -> String {
    let max_cols = usize::from(cols);
    let mut text = truncate_banner_text(BANNER_MENU_LABEL, cols);
    let priority_segments = [
        format!("agent={}", snapshot.agent),
        format!("state={}", snapshot.state),
        format!("activity={}", snapshot.activity),
        format!("host={}", snapshot.host),
        format!("id={}", snapshot.id),
    ];
    for segment in priority_segments {
        push_banner_segment(&mut text, &segment, max_cols, true);
    }

    let optional_segments = [
        format!("session={}", snapshot.name),
        format!("project={}", snapshot.project),
    ];
    for segment in optional_segments {
        push_banner_segment(&mut text, &segment, max_cols, false);
    }
    text
}

fn push_banner_segment(text: &mut String, segment: &str, max_cols: usize, required: bool) {
    if max_cols == 0 {
        return;
    }

    let current_cols = text.chars().count();
    let separator_cols = if text.is_empty() {
        0
    } else {
        BANNER_FIELD_SEPARATOR.chars().count()
    };
    let segment_cols = segment.chars().count();
    if current_cols + separator_cols + segment_cols <= max_cols {
        if separator_cols > 0 {
            text.push_str(BANNER_FIELD_SEPARATOR);
        }
        text.push_str(segment);
        return;
    }

    if !required || current_cols + separator_cols >= max_cols {
        return;
    }

    if separator_cols > 0 {
        text.push_str(BANNER_FIELD_SEPARATOR);
    }
    let remaining_cols = max_cols.saturating_sub(text.chars().count());
    text.extend(segment.chars().take(remaining_cols));
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
    initial: AttachStatusSnapshot,
) -> BannerUpdates {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = &mut cancel_rx => {}
            () = run_banner_updates(host, paths, session_id, initial, tx) => {}
        }
    });
    BannerUpdates {
        receiver: rx,
        cancel: Some(cancel_tx),
        task,
    }
}

async fn run_banner_updates(
    host: String,
    paths: Paths,
    session_id: String,
    mut snapshot: AttachStatusSnapshot,
    tx: mpsc::UnboundedSender<AttachStatusSnapshot>,
) {
    let Ok(mut client) = Client::connect(&host, &paths).await else {
        return;
    };

    if let Ok(request) =
        request_with_params(method::SESSION_INSPECT, &SessionId(session_id.clone()))
    {
        if let Ok(info) = client.request(&request).await {
            snapshot.update_from_session_value(&info);
            if tx.send(snapshot.clone()).is_err() {
                return;
            }
        }
    }

    let request = Request::new(
        request_id(method::SUBSCRIBE),
        method::SUBSCRIBE,
        serde_json::Value::Null,
    );
    let Ok(request) = request else {
        return;
    };
    let Ok(mut subscription) = client.into_sdk().subscribe(&request).await else {
        return;
    };
    while let Ok(Some(line)) = subscription.next_line().await {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let before = snapshot.clone();
        snapshot.update_from_event_value(&event);
        if snapshot != before && tx.send(snapshot.clone()).is_err() {
            return;
        }
    }
}

impl BannerUpdates {
    async fn shutdown(mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        let _ = self.task.await;
    }
}

async fn shutdown_banner_updates(updates: &mut Option<BannerUpdates>) {
    if let Some(updates) = updates.take() {
        updates.shutdown().await;
    }
}

async fn prepare_attach_terminal(
    updates: &mut Option<BannerUpdates>,
) -> Result<
    (
        Option<RawTerminal>,
        TerminalOutputGuard,
        tokio::signal::unix::Signal,
    ),
    CliError,
> {
    let terminal = match RawTerminal::enable(libc::STDIN_FILENO) {
        Ok(terminal) => terminal,
        Err(error) => {
            shutdown_banner_updates(updates).await;
            return Err(error);
        }
    };
    let winch = match signal(SignalKind::window_change()) {
        Ok(winch) => winch,
        Err(error) => {
            shutdown_banner_updates(updates).await;
            return Err(CliError::Io(error));
        }
    };
    // Output cleanup follows the physical output terminal independently of
    // whether stdin could enter raw mode (for example, piped stdin with a TTY
    // stdout can still receive mode-enabling agent output).
    let output = TerminalOutputGuard::new(is_tty(libc::STDOUT_FILENO));
    Ok((terminal, output, winch))
}

/// Renders the active modal and its status banner.
async fn paint_modal<W>(writer: &mut W, modal: &mut ModalState) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if !modal.active {
        return Ok(());
    }
    let text = render_banner_text(modal.cols, &modal.snapshot);
    let frame = modal.compositor.render(&text);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Restores passthrough and replays output received while the modal was open.
async fn close_modal<W>(writer: &mut W, modal: Option<&mut ModalState>) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    let Some(modal) = modal.filter(|modal| modal.active) else {
        return Ok(());
    };
    writer.write_all(&modal.compositor.restore()).await?;
    writer.write_all(&modal.pending_output).await?;
    writer.flush().await?;
    modal.pending_output.clear();
    modal.active = false;
    Ok(())
}

async fn restore_terminal_output_modes<W>(writer: &mut W) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(ATTACH_TERMINAL_MODE_CLEANUP).await?;
    writer.flush().await?;
    Ok(())
}

/// Opens the modal and paints its frozen background immediately.
async fn open_modal<W>(writer: &mut W, modal: Option<&mut ModalState>) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(modal) = modal {
        modal.active = true;
        paint_modal(writer, modal).await?;
    }
    Ok(())
}

/// Awaits the next banner snapshot, or never resolves when updates are absent.
///
/// Kept as a named future so the `select!` arm stays a single line and the loop
/// body fits its line budget.
async fn recv_banner_update(updates: &mut Option<BannerUpdates>) -> Option<AttachStatusSnapshot> {
    match updates.as_mut() {
        Some(updates) => updates.receiver.recv().await,
        None => None,
    }
}

async fn wait_for_menu_task(task: &mut Option<MenuTask>) -> Option<MenuTaskResult> {
    let Some(task) = task.as_mut() else {
        return std::future::pending().await;
    };
    let outcome = match (&mut task.handle).await {
        Ok(outcome) => outcome,
        Err(err) => Err(format!("menu action task failed: {err}")),
    };
    Some(MenuTaskResult {
        generation: task.generation,
        action: task.action,
        outcome,
    })
}

fn update_shortcut_deadline(decoder: &ShortcutDecoder, deadline: &mut Option<time::Instant>) {
    if decoder.has_pending() {
        *deadline = Some(time::Instant::now() + SHORTCUT_SEQUENCE_TIMEOUT);
    } else {
        *deadline = None;
    }
}

async fn read_stdin_event<R>(
    stdin: &mut R,
    buffer: &mut [u8],
    shortcut_deadline: Option<time::Instant>,
) -> std::io::Result<StdinEvent>
where
    R: AsyncRead + Unpin,
{
    let Some(deadline) = shortcut_deadline else {
        return stdin.read(buffer).await.map(StdinEvent::Input);
    };
    tokio::select! {
        read = stdin.read(buffer) => read.map(StdinEvent::Input),
        () = time::sleep_until(deadline) => Ok(StdinEvent::ShortcutTimeout),
    }
}

impl MenuRuntime {
    fn new(active: bool) -> Self {
        Self {
            state: active.then_some(MenuState::Closed),
            decoder: MenuInputDecoder::new(),
            generation: FIRST_MENU_GENERATION,
            task: None,
        }
    }
}

impl MenuInputDecoder {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    fn push(&mut self, input: &[u8]) -> Vec<MenuEvent> {
        let mut bytes = Vec::with_capacity(self.pending.len() + input.len());
        bytes.append(&mut self.pending);
        bytes.extend_from_slice(input);

        let mut events = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            let parsed = parse_menu_key(&bytes[index..]);
            if parsed.pending {
                self.pending.extend_from_slice(&bytes[index..]);
                break;
            }
            index += parsed.consumed;
            if let Some(key) = parsed.key {
                events.push(MenuEvent::Key(key));
            }
        }
        events
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedMenuKey {
    consumed: usize,
    pending: bool,
    key: Option<MenuKey>,
}

fn parse_menu_key(input: &[u8]) -> ParsedMenuKey {
    if input[0] != MENU_ESC_BYTE {
        return parse_plain_menu_key(input[0]);
    }
    if input.len() == 1 || input[1] != MENU_CSI_OPEN {
        return ParsedMenuKey {
            consumed: 1,
            pending: false,
            key: Some(MenuKey::Esc),
        };
    }
    if input.len() < MENU_ARROW_REPORT_LEN {
        return ParsedMenuKey {
            consumed: 0,
            pending: true,
            key: None,
        };
    }

    match input[MENU_CSI_PREFIX_LEN] {
        MENU_ARROW_UP => ParsedMenuKey {
            consumed: MENU_ARROW_REPORT_LEN,
            pending: false,
            key: Some(MenuKey::Up),
        },
        MENU_ARROW_DOWN => ParsedMenuKey {
            consumed: MENU_ARROW_REPORT_LEN,
            pending: false,
            key: Some(MenuKey::Down),
        },
        MENU_SGR_MOUSE_MARKER => parse_sgr_menu_mouse(input),
        MENU_LEGACY_MOUSE_MARKER => ParsedMenuKey {
            consumed: input.len().min(MENU_LEGACY_MOUSE_REPORT_LEN),
            pending: input.len() < MENU_LEGACY_MOUSE_REPORT_LEN,
            key: Some(MenuKey::Mouse),
        },
        _ => ParsedMenuKey {
            consumed: MENU_CSI_PREFIX_LEN,
            pending: false,
            key: None,
        },
    }
}

fn parse_plain_menu_key(byte: u8) -> ParsedMenuKey {
    let key = match byte {
        MENU_ENTER_CR | MENU_ENTER_LF => MenuKey::Enter,
        MENU_BACKSPACE_DEL | MENU_BACKSPACE_CTRL_H => MenuKey::Backspace,
        other => MenuKey::Byte(other),
    };
    ParsedMenuKey {
        consumed: 1,
        pending: false,
        key: Some(key),
    }
}

fn parse_sgr_menu_mouse(input: &[u8]) -> ParsedMenuKey {
    let final_index = input
        .iter()
        .position(|byte| matches!(*byte, MENU_SGR_EVENT_FINAL | MENU_SGR_RELEASE_FINAL));
    ParsedMenuKey {
        consumed: final_index.map_or(0, |index| index + 1),
        pending: final_index.is_none(),
        key: Some(MenuKey::Mouse),
    }
}

#[cfg(test)]
fn handle_menu_input_chunk(state: &mut MenuState, input: &[u8]) -> Vec<MenuEffect> {
    let mut decoder = MenuInputDecoder::new();
    handle_menu_input_chunk_with_decoder(state, &mut decoder, input).effects
}

fn handle_menu_input_chunk_with_decoder(
    state: &mut MenuState,
    decoder: &mut MenuInputDecoder,
    input: &[u8],
) -> MenuInputOutcome {
    let mut effects = Vec::new();
    let mut changed = false;
    for event in decoder.push(input) {
        let before = state.clone();
        let (next, mut next_effects) = step(before.clone(), event);
        changed |= next != before;
        *state = next;
        effects.append(&mut next_effects);
    }
    MenuInputOutcome { effects, changed }
}

async fn sync_menu_view<W>(
    menu_state: &MenuState,
    modal: &mut Option<ModalState>,
    stdout: &mut W,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(modal) = modal.as_mut() {
        modal.compositor.set_overlay(menu_state.to_overlay_frame());
        if *menu_state == MenuState::Closed {
            close_modal(stdout, Some(modal)).await?;
        } else if modal.active {
            paint_modal(stdout, modal).await?;
        } else {
            open_modal(stdout, Some(modal)).await?;
        }
    }
    Ok(())
}

fn next_menu_generation(generation: u64) -> u64 {
    generation.wrapping_add(MENU_GENERATION_STEP)
}

async fn handle_socket_output<W>(
    input: &[u8],
    stdout: &mut W,
    modal: &mut Option<ModalState>,
    menu: &mut MenuRuntime,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
{
    if input.is_empty() {
        return Ok(Some(AttachStreamEnd::StreamClosed));
    }
    if let Some(modal) = modal.as_mut() {
        modal.compositor.feed(input);
        if modal.active {
            if modal.pending_output.len().saturating_add(input.len()) > MAX_MODAL_BUFFER_BYTES {
                close_modal(stdout, Some(modal)).await?;
                if let Some(state) = menu.state.as_mut() {
                    *state = MenuState::Closed;
                }
                stdout.write_all(input).await?;
                stdout.flush().await?;
            } else {
                modal.pending_output.extend_from_slice(input);
            }
        } else {
            stdout.write_all(input).await?;
            stdout.flush().await?;
        }
    } else {
        stdout.write_all(input).await?;
        stdout.flush().await?;
    }
    Ok(None)
}

fn handle_status_update(snapshot: AttachStatusSnapshot, modal: &mut Option<ModalState>) -> bool {
    if let Some(modal) = modal.as_mut() {
        modal.snapshot = snapshot;
        return modal.active;
    }
    false
}

fn parse_positive_duration_seconds(
    key: &str,
    value: &str,
    path: &Path,
    number: usize,
) -> Result<Duration, CliError> {
    let seconds = value.parse::<f64>().map_err(|err| {
        config_error(format!(
            "{}:{number}: invalid duration value {value:?}: {err}",
            path.display()
        ))
    })?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(config_error(format!(
            "{}:{number}: {key} must be greater than zero",
            path.display()
        )));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_nonnegative_duration_seconds(
    key: &str,
    value: &str,
    path: &Path,
    number: usize,
) -> Result<Duration, CliError> {
    let seconds = value.parse::<f64>().map_err(|err| {
        config_error(format!(
            "{}:{number}: invalid duration value {value:?}: {err}",
            path.display()
        ))
    })?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(config_error(format!(
            "{}:{number}: {key} must be zero or greater",
            path.display()
        )));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_positive_usize(
    key: &str,
    value: &str,
    path: &Path,
    number: usize,
) -> Result<usize, CliError> {
    let count = value.parse::<usize>().map_err(|err| {
        config_error(format!(
            "{}:{number}: invalid positive integer value {value:?}: {err}",
            path.display()
        ))
    })?;
    if count == 0 {
        return Err(config_error(format!(
            "{}:{number}: {key} must be greater than zero",
            path.display()
        )));
    }
    Ok(count)
}

fn config_error(message: String) -> CliError {
    CliError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// Applies a window-size change to the shadow grid and daemon PTY.
///
/// An open modal is closed first because its background uses the old geometry.
async fn apply_terminal_resize<W>(
    client: &mut Client,
    target: &Target,
    modal: &mut Option<ModalState>,
    menu: &mut MenuRuntime,
    stdout: &mut W,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    let Some(dimensions) = current_terminal_dimensions() else {
        return Ok(());
    };
    apply_terminal_dimensions(client, target, modal, menu, stdout, dimensions).await
}

/// Applies known dimensions to the shadow grid and daemon PTY.
///
/// An open modal is closed first because its background uses the old geometry.
async fn apply_terminal_dimensions<W>(
    client: &mut Client,
    target: &Target,
    modal: &mut Option<ModalState>,
    menu: &mut MenuRuntime,
    stdout: &mut W,
    dimensions: TerminalDimensions,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    let cols = dimensions.cols();
    let rows = dimensions.rows();
    close_modal(stdout, modal.as_mut()).await?;
    if let Some(state) = menu.state.as_mut() {
        *state = MenuState::Closed;
    }
    if let Some(modal) = modal.as_mut() {
        modal.cols = cols;
        modal.compositor.resize(cols, rows);
    }
    if let Ok(request) = build_resize_request(target, cols, rows) {
        let _ = client.request(&request).await;
    }
    Ok(())
}

fn current_terminal_dimensions() -> Option<TerminalDimensions> {
    terminal_size(libc::STDOUT_FILENO)
        .and_then(|(cols, rows)| TerminalDimensions::new(cols, rows).ok())
}

fn resize_after_signal_registration(
    initial: Option<TerminalDimensions>,
    current: Option<TerminalDimensions>,
) -> Option<TerminalDimensions> {
    current.filter(|dimensions| Some(*dimensions) != initial)
}

async fn apply_resize_after_signal_registration<W>(
    client: &mut Client,
    target: &Target,
    ui: (&mut Option<ModalState>, &mut MenuRuntime, &mut W),
    initial_dimensions: Option<TerminalDimensions>,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    let (modal, menu, stdout) = ui;
    let Some(dimensions) =
        resize_after_signal_registration(initial_dimensions, current_terminal_dimensions())
    else {
        return Ok(());
    };
    apply_terminal_dimensions(client, target, modal, menu, stdout, dimensions).await
}

async fn handle_stdin_input<W, O>(
    input: &[u8],
    mut ctx: StdinInputContext<'_, W, O>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
    O: AsyncWrite + Unpin,
{
    let menu_mode = match ctx.menu.state.as_ref() {
        None => MenuInputMode::Unavailable,
        Some(MenuState::Closed) => MenuInputMode::Closed,
        Some(_) => MenuInputMode::Open,
    };
    let decoded = ctx.shortcuts.push(input, menu_mode);
    update_shortcut_deadline(ctx.shortcuts, ctx.shortcut_deadline);

    if let Some(shortcut) = decoded.shortcut {
        // Process only the prefix before the first local shortcut. Its suffix
        // is intentionally discarded to preserve the historical behavior.
        if let Some(end) = route_stdin_input(&decoded.bytes, ctx.reborrow()).await? {
            return Ok(Some(end));
        }
        match shortcut {
            Shortcut::Detach => {
                ctx.terminal.take();
                let _ = send_detach(ctx.client, ctx.stream_id).await;
                return Ok(Some(AttachStreamEnd::Detached));
            }
            Shortcut::Menu => {
                ctx.menu.generation = next_menu_generation(ctx.menu.generation);
                if let Some(state) = ctx.menu.state.as_mut() {
                    *state = MenuState::open_root();
                    sync_menu_view(state, ctx.modal, ctx.stdout).await?;
                }
                return Ok(None);
            }
        }
    }

    route_stdin_input(&decoded.bytes, ctx).await
}

async fn route_stdin_input<W, O>(
    input: &[u8],
    ctx: StdinInputContext<'_, W, O>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
    O: AsyncWrite + Unpin,
{
    if input.is_empty() {
        return Ok(None);
    }

    if let Some(state) = ctx
        .menu
        .state
        .as_mut()
        .filter(|state| **state != MenuState::Closed)
    {
        let outcome = handle_menu_input_chunk_with_decoder(state, &mut ctx.menu.decoder, input);
        if outcome.changed {
            sync_menu_view(state, ctx.modal, ctx.stdout).await?;
        }
        return handle_menu_effects(
            outcome.effects,
            MenuEffectContext {
                client: ctx.client,
                stream_id: ctx.stream_id,
                terminal: ctx.terminal,
                menu_task: &mut ctx.menu.task,
                generation: ctx.menu.generation,
                control: ctx.control,
                modal: ctx.modal.as_ref(),
            },
        )
        .await;
    }

    ctx.socket_write.write_all(input).await?;
    ctx.socket_write.flush().await?;
    Ok(None)
}

async fn handle_stdin_read<W, O>(
    bytes_read: usize,
    buffer: &mut [u8],
    mut ctx: StdinInputContext<'_, W, O>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
    O: AsyncWrite + Unpin,
{
    if bytes_read == 0 {
        *ctx.shortcut_deadline = None;
        let pending = ctx.shortcuts.flush();
        if let Some(end) = route_stdin_input(&pending, ctx.reborrow()).await? {
            return Ok(Some(end));
        }
        ctx.socket_write.shutdown().await?;
        return Ok(Some(AttachStreamEnd::InputClosed));
    }
    handle_stdin_input(&buffer[..bytes_read], ctx).await
}

async fn flush_shortcut_input<W, O>(
    ctx: StdinInputContext<'_, W, O>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
    O: AsyncWrite + Unpin,
{
    *ctx.shortcut_deadline = None;
    let pending = ctx.shortcuts.flush();
    route_stdin_input(&pending, ctx).await
}

async fn handle_stdin_event<W, O>(
    event: StdinEvent,
    buffer: &mut [u8],
    ctx: StdinInputContext<'_, W, O>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
    O: AsyncWrite + Unpin,
{
    match event {
        StdinEvent::Input(bytes_read) => handle_stdin_read(bytes_read, buffer, ctx).await,
        StdinEvent::ShortcutTimeout => flush_shortcut_input(ctx).await,
    }
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
    control: AttachControlContext,
    mut modal: Option<ModalState>,
    mut status_updates: Option<BannerUpdates>,
    initial_dimensions: Option<TerminalDimensions>,
) -> Result<AttachStreamEnd, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut terminal, mut terminal_output, mut winch) =
        prepare_attach_terminal(&mut status_updates).await?;
    let (mut socket_read, mut socket_write) = tokio::io::split(stream);
    let (mut stdin, mut stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let (mut stdin_buf, mut socket_buf) = ([0_u8; IO_BUFFER_BYTES], [0_u8; IO_BUFFER_BYTES]);
    let mut menu = MenuRuntime::new(modal.is_some());
    let mut shortcuts = ShortcutDecoder::new();
    let mut shortcut_deadline = None;

    apply_resize_after_signal_registration(
        &mut client,
        &control.target,
        (&mut modal, &mut menu, &mut stdout),
        initial_dimensions,
    )
    .await?;

    let outcome: Result<AttachStreamEnd, CliError> = async {
        loop {
            tokio::select! {
                read = socket_read.read(&mut socket_buf) => {
                    let bytes_read = read?;
                    if let Some(end) = handle_socket_output(
                        &socket_buf[..bytes_read],
                        &mut stdout,
                        &mut modal,
                        &mut menu,
                    )
                    .await?
                    {
                        return Ok(end);
                    }
                }
                event = read_stdin_event(
                    &mut stdin,
                    &mut stdin_buf,
                    shortcut_deadline,
                ) => {
                    let ctx = StdinInputContext {
                        socket_write: &mut socket_write,
                        stdout: &mut stdout,
                        client: &mut client,
                        stream_id: &stream_id,
                        terminal: &mut terminal,
                        modal: &mut modal,
                        menu: &mut menu,
                        shortcuts: &mut shortcuts,
                        shortcut_deadline: &mut shortcut_deadline,
                        control: &control,
                    };
                    let end = handle_stdin_event(event?, &mut stdin_buf, ctx).await?;
                    if let Some(end) = end {
                        return Ok(end);
                    }
                }
                resized = winch.recv() => {
                    if resized.is_none() {
                        continue;
                    }
                    apply_terminal_resize(
                        &mut client,
                        &control.target,
                        &mut modal,
                        &mut menu,
                        &mut stdout,
                    ).await?;
                }
                update = recv_banner_update(&mut status_updates), if status_updates.is_some() => {
                    if let Some(update) = update {
                        if handle_status_update(update, &mut modal) {
                            paint_modal(
                                &mut stdout,
                                modal.as_mut().expect("active modal state exists"),
                            ).await?;
                        }
                    } else {
                        shutdown_banner_updates(&mut status_updates).await;
                    }
                }
                task = wait_for_menu_task(&mut menu.task), if menu.task.is_some() => {
                    if let Some(end) = handle_menu_task_result(
                        task,
                        &control.target,
                        &mut menu.task,
                        &mut menu.state,
                        &mut modal,
                        &mut terminal,
                        &mut stdout,
                    ).await? {
                        return Ok(end);
                    }
                }
            }
        }
    }
    .await;

    // Best-effort restore; do not mask the loop's real outcome with a teardown
    // write error.
    let _ = close_modal(&mut stdout, modal.as_mut()).await;
    let _ = terminal_output.restore(&mut stdout).await;
    shutdown_banner_updates(&mut status_updates).await;
    finish_attach_outcome(&mut client, &stream_id, outcome).await
}

async fn finish_attach_outcome(
    client: &mut Client,
    stream_id: &str,
    outcome: Result<AttachStreamEnd, CliError>,
) -> Result<AttachStreamEnd, CliError> {
    if !matches!(outcome, Ok(AttachStreamEnd::StreamClosed)) {
        return outcome;
    }
    // The raw stream cannot carry a typed terminal failure. Ask the daemon once
    // through the still-open control connection. A daemon replacement may have
    // closed that connection too, so only a typed result overrides reconnect.
    if let Ok(detach) = send_detach(client, stream_id).await {
        if let Some(error) = stream_error_from_detach(detach) {
            return Err(error);
        }
    }
    outcome
}

fn stream_error_from_detach(detach: SessionDetachResult) -> Option<CliError> {
    detach.error.map(CliError::Protocol)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachReconnectDecision {
    Reattach,
    Retry,
    Finish,
    Fail,
}

async fn wait_for_attach_reconnect(
    host: &str,
    paths: &Paths,
    target: &Target,
    config: &AttachReconnectConfig,
    deadline: time::Instant,
    attempt: usize,
) -> Result<bool, CliError> {
    let now = time::Instant::now();
    if now >= deadline {
        return Ok(false);
    }
    let delay = reconnect_attempt_delay(config.interval, attempt, deadline - now);
    eprintln!(
        "[pohunek] attach stream closed; retrying session {} in {:.1}s \
         (attempt {} of {}, {:.1}s window remaining)",
        target.session_id,
        delay.as_secs_f64(),
        attempt,
        config.max_attempts,
        (deadline - now).as_secs_f64()
    );
    time::sleep(delay).await;

    loop {
        match probe_attach_reconnect(host, paths, target).await {
            Ok(AttachReconnectDecision::Reattach) => return Ok(true),
            Ok(AttachReconnectDecision::Finish | AttachReconnectDecision::Fail) => {
                return Ok(false);
            }
            Ok(AttachReconnectDecision::Retry) => {}
            Err(err) => match reconnect_decision_from_error(&err) {
                AttachReconnectDecision::Retry => {}
                AttachReconnectDecision::Finish => return Ok(false),
                AttachReconnectDecision::Reattach => return Ok(true),
                AttachReconnectDecision::Fail => return Err(err),
            },
        }

        let now = time::Instant::now();
        if now >= deadline {
            eprintln!(
                "[pohunek] attach reconnect window expired for session {}",
                target.session_id
            );
            return Ok(false);
        }
        time::sleep(std::cmp::min(config.interval, deadline - now)).await;
    }
}

fn reconnect_attempt_delay(interval: Duration, attempt: usize, remaining: Duration) -> Duration {
    let factor = u32::try_from(attempt).unwrap_or(u32::MAX);
    std::cmp::min(interval.saturating_mul(factor), remaining)
}

async fn probe_attach_reconnect(
    host: &str,
    paths: &Paths,
    target: &Target,
) -> Result<AttachReconnectDecision, CliError> {
    let mut client = Client::connect(host, paths).await?;
    let request = build_inspect_request(target)?;
    let result = client.request(&request).await?;
    let session: SessionInfo = serde_json::from_value(result)?;
    Ok(reconnect_decision_from_state(session.state))
}

fn reconnect_decision_from_state(state: SessionState) -> AttachReconnectDecision {
    match state {
        SessionState::Running => AttachReconnectDecision::Reattach,
        SessionState::Starting => AttachReconnectDecision::Retry,
        SessionState::Stopped | SessionState::Done | SessionState::Failed => {
            AttachReconnectDecision::Finish
        }
    }
}

fn reconnect_decision_from_error(err: &CliError) -> AttachReconnectDecision {
    match err {
        CliError::DaemonUnreachable { .. } => AttachReconnectDecision::Retry,
        CliError::Protocol(source)
            if matches!(
                source.code.as_str(),
                "session_not_found" | "session_not_running"
            ) =>
        {
            AttachReconnectDecision::Retry
        }
        CliError::Client(source)
            if matches!(
                source.to_protocol_error().code.as_str(),
                "daemon_unreachable" | "host_unreachable" | "remote_daemon_unavailable" | "framing"
            ) =>
        {
            AttachReconnectDecision::Retry
        }
        _ => AttachReconnectDecision::Fail,
    }
}

async fn handle_menu_effects(
    effects: Vec<MenuEffect>,
    ctx: MenuEffectContext<'_>,
) -> Result<Option<AttachStreamEnd>, CliError> {
    for effect in effects {
        match effect {
            MenuEffect::RunDetach => {
                ctx.terminal.take();
                let _ = send_detach(ctx.client, ctx.stream_id).await;
                return Ok(Some(AttachStreamEnd::Detached));
            }
            MenuEffect::RunKill => {
                if ctx.menu_task.is_none() {
                    *ctx.menu_task = Some(spawn_menu_task(
                        ctx.generation,
                        MenuTaskAction::Kill,
                        ctx.control,
                        MenuTaskArgs::Kill,
                    ));
                }
            }
            MenuEffect::RunNewSession => {
                if let (None, Some(modal)) = (ctx.menu_task.as_ref(), ctx.modal) {
                    let (rows, cols) = modal.compositor.grid_size();
                    *ctx.menu_task = Some(spawn_menu_task(
                        ctx.generation,
                        MenuTaskAction::NewSession,
                        ctx.control,
                        MenuTaskArgs::NewSession { cols, rows },
                    ));
                }
            }
            MenuEffect::RunFork => {
                if let (None, Some(modal)) = (ctx.menu_task.as_ref(), ctx.modal) {
                    let (rows, cols) = modal.compositor.grid_size();
                    *ctx.menu_task = Some(spawn_menu_task(
                        ctx.generation,
                        MenuTaskAction::Fork,
                        ctx.control,
                        MenuTaskArgs::Fork { cols, rows },
                    ));
                }
            }
            MenuEffect::RunRename(name) => {
                if ctx.menu_task.is_none() {
                    *ctx.menu_task = Some(spawn_menu_task(
                        ctx.generation,
                        MenuTaskAction::Rename,
                        ctx.control,
                        MenuTaskArgs::Rename { name },
                    ));
                }
            }
            MenuEffect::Close => {}
        }
    }
    Ok(None)
}

async fn handle_menu_task_result<W>(
    result: Option<MenuTaskResult>,
    target: &Target,
    menu_task: &mut Option<MenuTask>,
    menu_state: &mut Option<MenuState>,
    modal: &mut Option<ModalState>,
    terminal: &mut Option<RawTerminal>,
    stdout: &mut W,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
{
    let Some(result) = result else {
        return Ok(None);
    };
    *menu_task = None;
    if result.action == MenuTaskAction::Kill && result.outcome.is_ok() {
        terminal.take();
        return Ok(Some(AttachStreamEnd::SessionStopped));
    }

    let Some(state) = menu_state.as_mut() else {
        log_abandoned_menu_task_result(target, &result);
        return Ok(None);
    };
    if *state == MenuState::Closed {
        log_abandoned_menu_task_result(target, &result);
        return Ok(None);
    }

    let event = menu_event_from_task_outcome(result.outcome);
    let before = state.clone();
    let (next, effects) = step(before.clone(), event);
    *state = next;
    if *state != before {
        sync_menu_view(state, modal, stdout).await?;
    }
    debug_assert!(
        effects.is_empty(),
        "RPC completion events must not request new menu effects"
    );
    Ok(None)
}

fn menu_event_from_task_outcome(outcome: Result<MenuOutcome, String>) -> MenuEvent {
    match outcome {
        Ok(outcome) => MenuEvent::RpcDone(outcome),
        Err(message) => MenuEvent::RpcFailed(message),
    }
}

fn log_abandoned_menu_task_result(target: &Target, result: &MenuTaskResult) {
    match &result.outcome {
        Err(message) => {
            tracing::warn!(
                session_id = %target.session_id,
                action = result.action.as_str(),
                generation = result.generation,
                error = %message,
                "abandoned attach menu task failed"
            );
        }
        Ok(MenuOutcome::NewSession { id }) => {
            tracing::info!(
                session_id = %target.session_id,
                action = result.action.as_str(),
                generation = result.generation,
                created_session_id = %id,
                "abandoned attach menu task created a session"
            );
        }
        Ok(MenuOutcome::Forked { id }) => {
            tracing::info!(
                session_id = %target.session_id,
                action = result.action.as_str(),
                generation = result.generation,
                created_session_id = %id,
                "abandoned attach menu task forked a session"
            );
        }
        Ok(MenuOutcome::Renamed { name }) => {
            tracing::info!(
                session_id = %target.session_id,
                action = result.action.as_str(),
                generation = result.generation,
                name = name.as_deref().unwrap_or_default(),
                name_cleared = name.is_none(),
                "abandoned attach menu task renamed the session"
            );
        }
        Ok(MenuOutcome::Killed) => {}
    }
}

async fn send_detach(
    client: &mut Client,
    stream_id: &str,
) -> Result<SessionDetachResult, CliError> {
    let request = build_detach_request(stream_id)?;
    let value = client.request(&request).await?;
    Ok(serde_json::from_value(value)?)
}

async fn send_menu_new_session(
    client: &mut Client,
    target: &Target,
    cols: u16,
    rows: u16,
) -> Result<SessionNewResult, CliError> {
    let inspect = build_inspect_request(target)?;
    let value = client.request(&inspect).await?;
    let source: SessionInfo = serde_json::from_value(value)?;
    let request = build_menu_new_session_request(target, &source, cols, rows)?;
    let value = client.request(&request).await?;
    Ok(serde_json::from_value(value)?)
}

async fn send_menu_rename(
    client: &mut Client,
    target: &Target,
    name: String,
) -> Result<SessionRenameResult, CliError> {
    let request = build_menu_rename_request(target, &name)?;
    let value = client.request(&request).await?;
    Ok(serde_json::from_value(value)?)
}

async fn send_menu_fork(
    client: &mut Client,
    target: &Target,
    cols: u16,
    rows: u16,
) -> Result<SessionForkResult, CliError> {
    let request = build_menu_fork_request(target, cols, rows)?;
    let value = client.request(&request).await?;
    Ok(serde_json::from_value(value)?)
}

fn spawn_menu_task(
    generation: u64,
    action: MenuTaskAction,
    control: &AttachControlContext,
    args: MenuTaskArgs,
) -> MenuTask {
    let control = control.clone();
    let handle = tokio::spawn(async move {
        let mut client = Client::connect(&control.host, &control.paths)
            .await
            .map_err(|err| err.to_string())?;
        match args {
            MenuTaskArgs::Kill => {
                send_stop(&mut client, &control.target)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(MenuOutcome::Killed)
            }
            MenuTaskArgs::NewSession { cols, rows } => {
                let created = send_menu_new_session(&mut client, &control.target, cols, rows)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(MenuOutcome::NewSession {
                    id: created.session.id.0,
                })
            }
            MenuTaskArgs::Fork { cols, rows } => {
                let forked = send_menu_fork(&mut client, &control.target, cols, rows)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(MenuOutcome::Forked {
                    id: forked.session.id.0,
                })
            }
            MenuTaskArgs::Rename { name } => {
                let renamed = send_menu_rename(&mut client, &control.target, name)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(MenuOutcome::Renamed {
                    name: renamed.session.name,
                })
            }
        }
    });
    MenuTask {
        generation,
        action,
        handle,
    }
}

#[derive(Debug)]
enum MenuTaskArgs {
    Kill,
    NewSession { cols: u16, rows: u16 },
    Fork { cols: u16, rows: u16 },
    Rename { name: String },
}

async fn send_stop(client: &mut Client, target: &Target) -> Result<(), CliError> {
    let request = build_stop_request(target)?;
    let _ = client.request(&request).await?;
    Ok(())
}

fn build_stop_request(target: &Target) -> Result<Request, CliError> {
    request_with_params(method::SESSION_STOP, &SessionId(target.session_id.clone()))
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

#[derive(Debug)]
struct TerminalOutputGuard {
    enabled: bool,
    restored: bool,
}

impl TerminalOutputGuard {
    const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            restored: false,
        }
    }

    async fn restore<W>(&mut self, writer: &mut W) -> Result<(), CliError>
    where
        W: AsyncWrite + Unpin,
    {
        if self.restored || !self.enabled {
            self.restored = true;
            return Ok(());
        }
        restore_terminal_output_modes(writer).await?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalOutputGuard {
    fn drop(&mut self) {
        if self.enabled && !self.restored {
            let mut stdout = std::io::stdout().lock();
            let _ = std::io::Write::write_all(&mut stdout, ATTACH_TERMINAL_MODE_CLEANUP);
            let _ = std::io::Write::flush(&mut stdout);
        }
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use protocol::Response;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};
    use tokio::net::{UnixListener, UnixStream};

    use super::*;

    static NEXT_BANNER_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug)]
    struct BannerConnectionControl {
        release_event: oneshot::Sender<()>,
        closed: oneshot::Receiver<()>,
    }

    #[derive(Debug)]
    struct BannerTestDaemon {
        paths: Paths,
        controls: mpsc::UnboundedReceiver<BannerConnectionControl>,
        active: Arc<AtomicUsize>,
        task: JoinHandle<()>,
        socket: PathBuf,
    }

    #[derive(Debug)]
    struct StdinTestSocket {
        path: PathBuf,
    }

    impl Drop for StdinTestSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[derive(Debug)]
    struct StdinTestState {
        client: Client,
        stream_id: String,
        terminal: Option<RawTerminal>,
        modal: Option<ModalState>,
        menu: MenuRuntime,
        shortcuts: ShortcutDecoder,
        shortcut_deadline: Option<time::Instant>,
        control: AttachControlContext,
    }

    impl StdinTestState {
        fn new(client: Client, paths: Paths, menu_active: bool) -> Self {
            Self {
                client,
                stream_id: "stream-test".to_owned(),
                terminal: None,
                modal: None,
                menu: MenuRuntime::new(menu_active),
                shortcuts: ShortcutDecoder::new(),
                shortcut_deadline: None,
                control: AttachControlContext {
                    host: "local".to_owned(),
                    paths,
                    target: "local/s-42".parse().expect("test target"),
                },
            }
        }

        fn context<'a>(
            &'a mut self,
            socket_write: &'a mut DuplexStream,
            stdout: &'a mut Vec<u8>,
        ) -> StdinInputContext<'a, DuplexStream, Vec<u8>> {
            StdinInputContext {
                socket_write,
                stdout,
                client: &mut self.client,
                stream_id: &self.stream_id,
                terminal: &mut self.terminal,
                modal: &mut self.modal,
                menu: &mut self.menu,
                shortcuts: &mut self.shortcuts,
                shortcut_deadline: &mut self.shortcut_deadline,
                control: &self.control,
            }
        }
    }

    async fn connect_stdin_test_client() -> (Client, UnixStream, Paths, StdinTestSocket) {
        let id = NEXT_BANNER_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pohunek-cli-stdin-{}-{id}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind stdin test daemon");
        let paths = banner_test_paths(path.clone());
        let client = Client::connect("local", &paths)
            .await
            .expect("connect stdin test client");
        let (server, _) = listener.accept().await.expect("accept stdin test client");
        (
            client,
            server,
            paths,
            StdinTestSocket { path: path.clone() },
        )
    }

    #[tokio::test]
    async fn stdin_event_returns_available_input() {
        let (mut stdin, mut writer) = tokio::io::duplex(8);
        writer.write_all(b"x").await.expect("write stdin byte");
        let mut buffer = [0_u8; 8];

        let event = read_stdin_event(&mut stdin, &mut buffer, None)
            .await
            .expect("read stdin event");

        assert_eq!(event, StdinEvent::Input(1));
        assert_eq!(&buffer[..1], b"x");
    }

    #[tokio::test]
    async fn stdin_event_reports_expired_shortcut_timeout() {
        let (mut stdin, _writer) = tokio::io::duplex(8);
        let mut buffer = [0_u8; 8];

        let event = read_stdin_event(&mut stdin, &mut buffer, Some(time::Instant::now()))
            .await
            .expect("wait for shortcut timeout");

        assert_eq!(event, StdinEvent::ShortcutTimeout);
    }

    #[test]
    fn shortcut_deadline_refreshes_after_each_pending_chunk() {
        let mut decoder = ShortcutDecoder::new();
        let first = decoder.push(b"\x1b[92;", MenuInputMode::Closed);
        assert!(first.bytes.is_empty());
        assert!(decoder.has_pending());
        let stale_deadline = time::Instant::now();
        let mut deadline = Some(stale_deadline);

        let second = decoder.push(b"5", MenuInputMode::Closed);
        update_shortcut_deadline(&decoder, &mut deadline);

        assert!(second.bytes.is_empty());
        assert!(decoder.has_pending());
        assert!(deadline.is_some_and(|deadline| deadline > stale_deadline));
    }

    #[tokio::test]
    async fn stdin_eof_routes_pending_input_before_shutdown() {
        let (client, _server, paths, _socket) = connect_stdin_test_client().await;
        let mut state = StdinTestState::new(client, paths, false);
        let decoded = state
            .shortcuts
            .push(b"\x1b[92;", MenuInputMode::Unavailable);
        assert!(decoded.bytes.is_empty());
        state.shortcut_deadline = Some(time::Instant::now());
        let (mut socket_write, mut socket_read) = tokio::io::duplex(64);
        let mut stdout = Vec::new();
        let mut buffer = [];

        let end = handle_stdin_read(
            0,
            &mut buffer,
            state.context(&mut socket_write, &mut stdout),
        )
        .await
        .expect("handle stdin EOF");
        let mut forwarded = Vec::new();
        socket_read
            .read_to_end(&mut forwarded)
            .await
            .expect("read forwarded pending bytes");

        assert_eq!(end, Some(AttachStreamEnd::InputClosed));
        assert_eq!(forwarded, b"\x1b[92;");
        assert!(!state.shortcuts.has_pending());
        assert_eq!(state.shortcut_deadline, None);
    }

    #[tokio::test]
    async fn shortcut_timeout_routes_pending_input() {
        let (client, _server, paths, _socket) = connect_stdin_test_client().await;
        let mut state = StdinTestState::new(client, paths, false);
        let decoded = state
            .shortcuts
            .push(b"\x1b[92;", MenuInputMode::Unavailable);
        assert!(decoded.bytes.is_empty());
        state.shortcut_deadline = Some(time::Instant::now());
        let (mut socket_write, mut socket_read) = tokio::io::duplex(64);
        let mut stdout = Vec::new();

        let end = flush_shortcut_input(state.context(&mut socket_write, &mut stdout))
            .await
            .expect("flush timed-out shortcut input");
        let mut forwarded = [0_u8; 5];
        socket_read
            .read_exact(&mut forwarded)
            .await
            .expect("read timed-out input");

        assert_eq!(end, None);
        assert_eq!(&forwarded, b"\x1b[92;");
        assert!(!state.shortcuts.has_pending());
        assert_eq!(state.shortcut_deadline, None);
    }

    #[tokio::test]
    async fn csi_u_detach_ends_attach_and_sends_control_request() {
        let (client, server, paths, _socket) = connect_stdin_test_client().await;
        let server_task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let request = read_banner_test_request(&mut server).await;
            assert_request(
                &request,
                method::SESSION_DETACH,
                json!({"stream_id": "stream-test"}),
            );
            write_banner_test_response(
                &mut server,
                &request,
                json!({"detached": true, "error": null}),
            )
            .await;
        });
        let mut state = StdinTestState::new(client, paths, false);
        let (mut socket_write, _socket_read) = tokio::io::duplex(64);
        let mut stdout = Vec::new();

        let end = handle_stdin_input(b"\x1b[93;5u", state.context(&mut socket_write, &mut stdout))
            .await
            .expect("handle CSI-u detach");
        server_task.await.expect("detach test daemon");

        assert_eq!(end, Some(AttachStreamEnd::Detached));
        assert!(!state.shortcuts.has_pending());
    }

    #[tokio::test]
    async fn csi_u_escape_closes_open_menu_without_leaking_text() {
        let (client, _server, paths, _socket) = connect_stdin_test_client().await;
        let mut state = StdinTestState::new(client, paths, true);
        state.menu.state = Some(MenuState::open_root());
        let (mut socket_write, mut socket_read) = tokio::io::duplex(64);
        let mut stdout = Vec::new();

        let end = handle_stdin_input(b"\x1b[27u", state.context(&mut socket_write, &mut stdout))
            .await
            .expect("handle CSI-u Escape");
        drop(socket_write);
        let mut leaked = Vec::new();
        socket_read
            .read_to_end(&mut leaked)
            .await
            .expect("read attached PTY input");

        assert_eq!(end, None);
        assert_eq!(state.menu.state, Some(MenuState::Closed));
        assert!(leaked.is_empty());
    }

    #[test]
    fn csi_u_escape_cancels_rename_without_appending_report_text() {
        let mut decoder = ShortcutDecoder::new();
        let mut state = MenuState::RenameInput {
            buffer: "kept".to_owned(),
        };

        let escape = decoder.push(b"\x1b[27u", MenuInputMode::Open);
        let effects = handle_menu_input_chunk(&mut state, &escape.bytes);

        assert_eq!(state, MenuState::open_root());
        assert!(effects.is_empty());
    }

    #[test]
    fn csi_u_enter_and_backspace_are_canonical_menu_keys() {
        let mut decoder = ShortcutDecoder::new();
        let mut state = MenuState::RenameInput {
            buffer: "ab".to_owned(),
        };

        let backspace = decoder.push(b"\x1b[127u", MenuInputMode::Open);
        let backspace_effects = handle_menu_input_chunk(&mut state, &backspace.bytes);
        assert_eq!(
            state,
            MenuState::RenameInput {
                buffer: "a".to_owned()
            }
        );
        assert!(backspace_effects.is_empty());

        let enter = decoder.push(b"\x1b[13u", MenuInputMode::Open);
        let enter_effects = handle_menu_input_chunk(&mut state, &enter.bytes);
        assert_eq!(
            state,
            MenuState::Busy {
                label: "Renaming session".to_owned()
            }
        );
        assert_eq!(enter_effects, vec![MenuEffect::RunRename("a".to_owned())]);
    }

    impl Drop for BannerTestDaemon {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn banner_test_paths(socket: PathBuf) -> Paths {
        let root = socket
            .parent()
            .expect("test socket has a parent")
            .to_path_buf();
        Paths {
            runtime_dir: root.clone(),
            socket,
            data_dir: root.join("data"),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            config_home: root.join("config-home"),
            config_dir: root.join("config"),
        }
    }

    fn spawn_banner_test_daemon(connection_count: usize) -> BannerTestDaemon {
        let id = NEXT_BANNER_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
        let socket = std::env::temp_dir().join(format!(
            "pohunek-cli-banner-{}-{id}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind banner test daemon");
        let (controls_tx, controls) = mpsc::unbounded_channel();
        let active = Arc::new(AtomicUsize::new(0));
        let task_active = Arc::clone(&active);
        let task = tokio::spawn(async move {
            let mut connections = Vec::with_capacity(connection_count);
            for _ in 0..connection_count {
                let (stream, _) = listener.accept().await.expect("accept banner client");
                let tx = controls_tx.clone();
                let active = Arc::clone(&task_active);
                connections.push(tokio::spawn(async move {
                    handle_banner_test_connection(stream, tx, active).await;
                }));
            }
            drop(controls_tx);
            for connection in connections {
                connection.await.expect("banner connection task");
            }
        });

        BannerTestDaemon {
            paths: banner_test_paths(socket.clone()),
            controls,
            active,
            task,
            socket,
        }
    }

    async fn handle_banner_test_connection(
        stream: UnixStream,
        controls: mpsc::UnboundedSender<BannerConnectionControl>,
        active: Arc<AtomicUsize>,
    ) {
        let mut stream = BufReader::new(stream);
        let inspect = read_banner_test_request(&mut stream).await;
        write_banner_test_response(
            &mut stream,
            &inspect,
            json!({
                "id": "s-42",
                "name": "test",
                "agent": "claude",
                "state": "running",
                "activity": "idle"
            }),
        )
        .await;
        let subscribe = read_banner_test_request(&mut stream).await;
        write_banner_test_response(&mut stream, &subscribe, json!({"subscribed": true})).await;

        let (release_event, release_rx) = oneshot::channel();
        let (closed_tx, closed) = oneshot::channel();
        active.fetch_add(1, Ordering::SeqCst);
        controls
            .send(BannerConnectionControl {
                release_event,
                closed,
            })
            .expect("send banner connection control");

        let mut eof = String::new();
        tokio::select! {
            _ = release_rx => {
                let event = serde_json::to_string(&json!({
                    "v": 1,
                    "event": event::AGENT_STATE,
                    "session_id": "s-42",
                    "activity": "working"
                }))
                .expect("serialize banner event");
                let _ = stream.get_mut().write_all(event.as_bytes()).await;
                let _ = stream.get_mut().write_all(b"\n").await;
                let _ = stream.get_mut().flush().await;
                let _ = stream.read_line(&mut eof).await;
            }
            _ = stream.read_line(&mut eof) => {}
        }
        active.fetch_sub(1, Ordering::SeqCst);
        let _ = closed_tx.send(());
    }

    async fn read_banner_test_request(stream: &mut BufReader<UnixStream>) -> Request {
        let mut line = String::new();
        stream
            .read_line(&mut line)
            .await
            .expect("read banner request");
        serde_json::from_str(&line).expect("decode banner request")
    }

    async fn write_banner_test_response(
        stream: &mut BufReader<UnixStream>,
        request: &Request,
        value: serde_json::Value,
    ) {
        let line = serde_json::to_string(
            &Response::ok(
                request.version_range().maximum(),
                request.id().to_owned(),
                value,
            )
            .expect("valid banner response"),
        )
        .expect("serialize banner response");
        stream
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .expect("write banner response");
        stream
            .get_mut()
            .write_all(b"\n")
            .await
            .expect("write banner response newline");
        stream
            .get_mut()
            .flush()
            .await
            .expect("flush banner response");
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "test helper takes the json! literal by value to keep call sites terse"
    )]
    fn assert_request(request: &Request, method_name: &str, params: serde_json::Value) {
        assert_eq!(
            request.version_range(),
            protocol::SUPPORTED_PROTOCOL_VERSIONS
        );
        assert_eq!(request.method(), method_name, "method");
        assert_eq!(request.params(), &params, "params");
        // The id is now a unique per-call SDK correlation id; assert only its
        // stable, log-greppable `sdk-<method>-` prefix.
        assert!(
            request.id().starts_with(&format!("sdk-{method_name}-")),
            "id {:?} must be prefixed by the method",
            request.id()
        );
    }

    #[test]
    fn attach_request_sends_session_id() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_attach_request(&target, None, None, None, None).expect("request");

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
            Some(TerminalDimensions::new(120, 40).expect("valid dimensions")),
            Some(SessionId("s-42".to_owned())),
            Some("daemon-xyz".to_owned()),
            Some("worker-xyz".to_owned()),
        )
        .expect("request");

        assert_request(
            &request,
            method::SESSION_ATTACH,
            json!({
                "session_id": "s-42",
                "initial_dimensions": { "cols": 120, "rows": 40 },
                "origin_session_id": "s-42",
                "origin_daemon_id": "daemon-xyz",
                "origin_worker_id": "worker-xyz"
            }),
        );
    }

    #[test]
    fn self_feedback_origin_reports_stable_worker_when_present() {
        // Inside a session's PTY: report both so the daemon can pin the loop.
        assert_eq!(
            self_feedback_origin_from(
                Some("s-7".to_owned()),
                Some("daemon-1".to_owned()),
                Some("worker-1".to_owned())
            ),
            (
                Some(SessionId("s-7".to_owned())),
                Some("daemon-1".to_owned()),
                Some("worker-1".to_owned())
            )
        );
        // Not inside any session (env unset or empty): nothing to report.
        assert_eq!(
            self_feedback_origin_from(None, None, None),
            (None, None, None)
        );
        assert_eq!(
            self_feedback_origin_from(
                Some(String::new()),
                Some(String::new()),
                Some(String::new())
            ),
            (None, None, None)
        );
        // A session id without a daemon id cannot be pinned to an instance; it is
        // still forwarded, and the daemon declines to reject without a daemon id.
        assert_eq!(
            self_feedback_origin_from(Some("s-7".to_owned()), None, None),
            (Some(SessionId("s-7".to_owned())), None, None)
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
        let request = build_attach_request(&remote, None, None, None, None).expect("request");

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
    fn resize_after_signal_registration_only_applies_changed_dimensions() {
        let initial = TerminalDimensions::new(120, 40).expect("initial dimensions");
        let changed = TerminalDimensions::new(100, 30).expect("changed dimensions");

        assert_eq!(
            resize_after_signal_registration(Some(initial), Some(initial)),
            None
        );
        assert_eq!(
            resize_after_signal_registration(Some(initial), Some(changed)),
            Some(changed)
        );
        assert_eq!(
            resize_after_signal_registration(None, Some(changed)),
            Some(changed)
        );
        assert_eq!(resize_after_signal_registration(Some(initial), None), None);
    }

    #[test]
    fn attach_config_reads_reconnect_values() {
        let root = std::env::temp_dir().join(format!(
            "pohunek-attach-banner-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create config dir");
        std::fs::write(
            root.join("launcher.conf"),
            "attach_reconnect_seconds=12\n\
             attach_reconnect_interval_seconds=0.75\n\
             attach_reconnect_max_attempts=7\n",
        )
        .expect("write config");

        let config = AttachConfig::load_from_config_dir(&root).expect("load config");

        assert_eq!(config.reconnect.window, std::time::Duration::from_secs(12));
        assert_eq!(
            config.reconnect.interval,
            std::time::Duration::from_millis(750)
        );
        assert_eq!(config.reconnect.max_attempts, 7);
    }

    #[test]
    fn attach_config_rejects_zero_reconnect_attempts() {
        let root = std::env::temp_dir().join(format!(
            "pohunek-attach-reconnect-attempts-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create config dir");
        std::fs::write(
            root.join("launcher.conf"),
            "attach_reconnect_max_attempts=0\n",
        )
        .expect("write config");

        let err = AttachConfig::load_from_config_dir(&root)
            .expect_err("zero reconnect attempts must fail configuration validation");

        assert!(
            err.to_string().contains("must be greater than zero"),
            "err: {err}"
        );
    }

    #[test]
    fn attach_reconnect_budget_keeps_one_deadline_and_caps_attempts() {
        let config = AttachReconnectConfig {
            window: Duration::from_secs(20),
            interval: Duration::from_millis(500),
            max_attempts: 3,
        };
        let now = time::Instant::now();
        let mut budget = AttachReconnectBudget::default();

        let first = budget.next(&config, now);
        let deadline = match first {
            AttachReconnectPermit::Retry {
                deadline,
                attempt: 1,
            } => deadline,
            other => panic!("expected first retry permit, got {other:?}"),
        };
        assert_eq!(
            budget.next(&config, now + Duration::from_secs(1)),
            AttachReconnectPermit::Retry {
                deadline,
                attempt: 2
            }
        );
        assert_eq!(
            budget.next(&config, now + Duration::from_secs(2)),
            AttachReconnectPermit::Retry {
                deadline,
                attempt: 3
            }
        );
        assert_eq!(
            budget.next(&config, now + Duration::from_secs(3)),
            AttachReconnectPermit::AttemptsExhausted
        );
    }

    #[test]
    fn attach_reconnect_budget_expires_original_window() {
        let config = AttachReconnectConfig {
            window: Duration::from_secs(2),
            interval: Duration::from_millis(500),
            max_attempts: 10,
        };
        let now = time::Instant::now();
        let mut budget = AttachReconnectBudget::default();
        assert!(matches!(
            budget.next(&config, now),
            AttachReconnectPermit::Retry { attempt: 1, .. }
        ));

        assert_eq!(
            budget.next(&config, now + config.window),
            AttachReconnectPermit::WindowExpired
        );
    }

    #[test]
    fn attach_reconnect_delay_backs_off_and_stays_within_window() {
        let interval = Duration::from_millis(500);

        assert_eq!(
            reconnect_attempt_delay(interval, 1, Duration::from_secs(20)),
            Duration::from_millis(500)
        );
        assert_eq!(
            reconnect_attempt_delay(interval, 3, Duration::from_secs(20)),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            reconnect_attempt_delay(interval, 3, Duration::from_millis(800)),
            Duration::from_millis(800)
        );
    }

    #[test]
    fn attach_reconnect_decision_retries_only_live_or_temporarily_missing_sessions() {
        assert_eq!(
            reconnect_decision_from_state(protocol::SessionState::Running),
            AttachReconnectDecision::Reattach
        );
        assert_eq!(
            reconnect_decision_from_state(protocol::SessionState::Starting),
            AttachReconnectDecision::Retry
        );
        assert_eq!(
            reconnect_decision_from_state(protocol::SessionState::Done),
            AttachReconnectDecision::Finish
        );

        let missing = protocol::ProtocolError::new(
            protocol::ErrorClass::Runtime,
            "session_not_found",
            "session vanished during restart",
            None,
        );
        assert_eq!(
            reconnect_decision_from_error(&CliError::Protocol(missing)),
            AttachReconnectDecision::Retry
        );
    }

    #[test]
    fn detach_stream_error_is_typed_and_not_reconnectable() {
        let source = protocol::ProtocolError::new(
            protocol::ErrorClass::Transport,
            "worker_stream_failed",
            "worker data payload exceeded the frame limit",
            None,
        );
        let err = stream_error_from_detach(SessionDetachResult {
            detached: false,
            error: Some(source.clone()),
        })
        .expect("stream failure must surface");

        assert!(matches!(
            &err,
            CliError::Protocol(protocol_error) if protocol_error == &source
        ));
        assert_eq!(
            reconnect_decision_from_error(&err),
            AttachReconnectDecision::Fail
        );
    }

    #[tokio::test]
    async fn banner_shutdown_returns_subscription_connections_to_baseline() {
        const ATTEMPTS: usize = 3;
        let mut daemon = spawn_banner_test_daemon(ATTEMPTS);

        for _ in 0..ATTEMPTS {
            let updates = spawn_banner_updates(
                "local".to_owned(),
                daemon.paths.clone(),
                "s-42".to_owned(),
                AttachStatusSnapshot::unknown("local", "s-42"),
            );
            let control = time::timeout(Duration::from_secs(2), daemon.controls.recv())
                .await
                .expect("banner subscription established promptly")
                .expect("banner daemon control remains open");
            assert_eq!(daemon.active.load(Ordering::SeqCst), 1);

            updates.shutdown().await;
            time::timeout(Duration::from_secs(2), control.closed)
                .await
                .expect("banner connection closes after shutdown")
                .expect("banner connection reports close");
            assert_eq!(daemon.active.load(Ordering::SeqCst), 0);
        }

        time::timeout(Duration::from_secs(2), &mut daemon.task)
            .await
            .expect("banner daemon exits after all attempts")
            .expect("banner daemon task succeeds");
    }

    #[tokio::test]
    async fn dropping_banner_receiver_terminates_subscription_task() {
        let mut daemon = spawn_banner_test_daemon(1);
        let updates = spawn_banner_updates(
            "local".to_owned(),
            daemon.paths.clone(),
            "s-42".to_owned(),
            AttachStatusSnapshot::unknown("local", "s-42"),
        );
        let control = time::timeout(Duration::from_secs(2), daemon.controls.recv())
            .await
            .expect("banner subscription established promptly")
            .expect("banner daemon control remains open");
        let BannerUpdates {
            receiver,
            cancel,
            task,
        } = updates;
        drop(receiver);
        control
            .release_event
            .send(())
            .expect("release banner event");

        time::timeout(Duration::from_secs(2), task)
            .await
            .expect("dropped receiver terminates banner task")
            .expect("banner task exits normally");
        drop(cancel);
        time::timeout(Duration::from_secs(2), control.closed)
            .await
            .expect("banner connection closes after receiver drop")
            .expect("banner connection reports close");
        assert_eq!(daemon.active.load(Ordering::SeqCst), 0);
        time::timeout(Duration::from_secs(2), &mut daemon.task)
            .await
            .expect("banner daemon exits")
            .expect("banner daemon task succeeds");
    }

    #[test]
    fn attach_banner_text_prioritizes_live_state_and_menu_shortcut() {
        let mut snapshot = AttachStatusSnapshot::unknown("local", "s-42");
        snapshot.update_from_session_value(&json!({
            "id": "s-42",
            "name": "review branch",
            "agent": "claude",
            "state": "running",
            "activity": "blocked",
            "project_id": "p-abc123",
            "project_label": "ui"
        }));

        let text = render_banner_text(120, &snapshot);

        assert_eq!(
            text,
            "[menu:Ctrl-\\]  agent=claude  state=running  activity=blocked  host=local  id=s-42  session=review branch  project=ui",
            "banner text must start with the menu action and prioritize live state: {text:?}"
        );
    }

    #[test]
    fn attach_banner_narrow_width_keeps_live_state_visible() {
        let mut snapshot = AttachStatusSnapshot::unknown("local", "s-37");
        snapshot.update_from_session_value(&json!({
            "id": "s-37",
            "name": "debug s31",
            "agent": "codex",
            "state": "running",
            "activity": "blocked",
            "project_id": "p-8d0114ca"
        }));
        let text = render_banner_text(80, &snapshot);

        assert!(
            text.contains("agent=codex  state=running  activity=blocked"),
            "narrow banner must preserve the live state before lower-priority labels: {text:?}"
        );
        assert!(
            text.chars().count() <= 80,
            "banner text must fit the declared terminal width: {text:?}"
        );
    }

    #[test]
    fn attach_menu_open_state_swallows_mouse_reports() {
        let mut state = MenuState::open_root();

        let effects = handle_menu_input_chunk(&mut state, b"\x1b[<64;7;2M");

        assert_eq!(state, MenuState::Root { selected: 0 });
        assert!(effects.is_empty());
    }

    #[test]
    fn attach_menu_open_state_routes_keyboard_to_effects() {
        let mut state = MenuState::open_root();

        let effects = handle_menu_input_chunk(&mut state, b"d");

        assert_eq!(state, MenuState::Closed);
        assert_eq!(effects, vec![MenuEffect::RunDetach, MenuEffect::Close]);
    }

    #[test]
    fn attach_menu_open_state_routes_fork_hotkey_to_effects() {
        let mut state = MenuState::open_root();

        let effects = handle_menu_input_chunk(&mut state, b"f");

        assert_eq!(
            state,
            MenuState::Busy {
                label: "Forking session".to_owned()
            }
        );
        assert_eq!(effects, vec![MenuEffect::RunFork]);
    }

    fn placeholder_menu_task(generation: u64, action: MenuTaskAction) -> MenuTask {
        MenuTask {
            generation,
            action,
            handle: tokio::spawn(async { Ok(MenuOutcome::Killed) }),
        }
    }

    #[tokio::test]
    async fn menu_task_result_from_abandoned_generation_resolves_reopened_busy() {
        let old_generation = FIRST_MENU_GENERATION;
        let mut menu_task = Some(placeholder_menu_task(
            old_generation,
            MenuTaskAction::NewSession,
        ));
        let mut menu_state = Some(MenuState::Busy {
            label: "Starting session".to_owned(),
        });
        let mut modal = None;
        let mut terminal = None;
        let mut stdout = Vec::new();
        let target: Target = "host-a/s-42".parse().expect("target");

        let end = handle_menu_task_result(
            Some(MenuTaskResult {
                generation: old_generation,
                action: MenuTaskAction::NewSession,
                outcome: Ok(MenuOutcome::NewSession {
                    id: "s-created".to_owned(),
                }),
            }),
            &target,
            &mut menu_task,
            &mut menu_state,
            &mut modal,
            &mut terminal,
            &mut stdout,
        )
        .await
        .expect("handle task result");

        assert_eq!(end, None);
        assert_eq!(
            menu_state,
            Some(MenuState::Result {
                message: "New session created: s-created".to_owned()
            })
        );
        assert!(
            menu_task.is_none(),
            "completed menu task must clear the slot"
        );
    }

    #[tokio::test]
    async fn closed_menu_task_failure_keeps_modal_closed_and_clears_task() {
        let generation = FIRST_MENU_GENERATION;
        let mut menu_task = Some(placeholder_menu_task(generation, MenuTaskAction::Kill));
        let mut menu_state = Some(MenuState::Closed);
        let mut modal = None;
        let mut terminal = None;
        let mut stdout = Vec::new();
        let target: Target = "host-a/s-42".parse().expect("target");

        let end = handle_menu_task_result(
            Some(MenuTaskResult {
                generation,
                action: MenuTaskAction::Kill,
                outcome: Err("stop failed".to_owned()),
            }),
            &target,
            &mut menu_task,
            &mut menu_state,
            &mut modal,
            &mut terminal,
            &mut stdout,
        )
        .await
        .expect("handle task result");

        assert_eq!(end, None);
        assert_eq!(menu_state, Some(MenuState::Closed));
        assert!(
            menu_task.is_none(),
            "failed abandoned task must clear the slot"
        );
    }

    #[tokio::test]
    async fn menu_task_result_surfaces_when_modal_is_open_but_not_busy() {
        let generation = FIRST_MENU_GENERATION;
        let mut menu_task = Some(placeholder_menu_task(generation, MenuTaskAction::Fork));
        let mut menu_state = Some(MenuState::open_root());
        let mut modal = None;
        let mut terminal = None;
        let mut stdout = Vec::new();
        let target: Target = "host-a/s-42".parse().expect("target");

        let end = handle_menu_task_result(
            Some(MenuTaskResult {
                generation,
                action: MenuTaskAction::Fork,
                outcome: Ok(MenuOutcome::Forked {
                    id: "s-forked".to_owned(),
                }),
            }),
            &target,
            &mut menu_task,
            &mut menu_state,
            &mut modal,
            &mut terminal,
            &mut stdout,
        )
        .await
        .expect("handle task result");

        assert_eq!(end, None);
        assert_eq!(
            menu_state,
            Some(MenuState::Result {
                message: "Forked session created: s-forked".to_owned()
            })
        );
        assert!(
            menu_task.is_none(),
            "completed menu task must clear the slot"
        );
    }

    #[test]
    fn menu_new_session_request_reuses_source_worktree_and_agent() {
        let target: Target = "host-a/s-42".parse().expect("target");
        let source: SessionInfo = serde_json::from_value(json!({
            "id": "s-42",
            "external": false,
            "agent": "claude",
            "agent_base": "claude",
            "cwd": "/work/tree",
            "pid": 4242,
            "cols": 120,
            "rows": 39,
            "state": "running",
            "state_source": "process",
            "created_at": "2026-07-07T00:00:00Z",
            "updated_at": "2026-07-07T00:00:00Z"
        }))
        .expect("session info");

        let request = build_menu_new_session_request(&target, &source, 100, 30).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "claude",
                "cwd": "/work/tree",
                "cols": 100,
                "rows": 30
            }),
        );
    }

    #[test]
    fn menu_rename_request_sends_session_id_and_name() {
        let target: Target = "host-a/s-42".parse().expect("target");

        let request = build_menu_rename_request(&target, "review branch").expect("request");

        assert_request(
            &request,
            method::SESSION_RENAME,
            json!({
                "session_id": "s-42",
                "name": "review branch"
            }),
        );
    }

    #[test]
    fn menu_fork_request_sends_session_id_size_and_same_cwd_mode() {
        let target: Target = "host-a/s-42".parse().expect("target");

        let request = build_menu_fork_request(&target, 100, 30).expect("request");

        assert_request(
            &request,
            method::SESSION_FORK,
            json!({
                "session_id": "s-42",
                "cwd_mode": "same",
                "cols": 100,
                "rows": 30
            }),
        );
    }

    #[tokio::test]
    async fn attach_modal_buffers_and_replays_agent_output() {
        let mut modal = Some(ModalState {
            compositor: Compositor::new(80, 24),
            snapshot: AttachStatusSnapshot::unknown("local", "s-42"),
            cols: 80,
            pending_output: Vec::new(),
            active: false,
        });
        let mut stdout = Vec::new();
        let mut menu = MenuRuntime::new(true);

        handle_socket_output(b"before", &mut stdout, &mut modal, &mut menu)
            .await
            .expect("forward passthrough output");
        assert_eq!(stdout, b"before");

        {
            let state = modal.as_mut().expect("modal state");
            state
                .compositor
                .set_overlay(MenuState::open_root().to_overlay_frame());
            open_modal(&mut stdout, Some(state))
                .await
                .expect("open modal");
        };
        stdout.clear();

        let during = b"\x1b[?1049h\x1b[2;20r\x1b[?1006hduring";
        handle_socket_output(during, &mut stdout, &mut modal, &mut menu)
            .await
            .expect("buffer modal output");
        assert!(
            stdout.is_empty(),
            "agent output must stay hidden during modal"
        );

        let state = modal.as_mut().expect("modal state");
        close_modal(&mut stdout, Some(state))
            .await
            .expect("close modal");
        assert!(
            stdout.ends_with(during),
            "buffered screen, margin, and mouse modes must replay byte-for-byte"
        );
        assert!(state.pending_output.is_empty());
        assert!(!state.active);
    }

    #[tokio::test]
    async fn unexpected_attach_eof_emits_terminal_mode_cleanup() {
        let mut stdout = Vec::new();
        let mut modal = None;
        let mut menu = MenuRuntime::new(false);
        let mut output = TerminalOutputGuard::new(true);

        let end = handle_socket_output(&[], &mut stdout, &mut modal, &mut menu)
            .await
            .expect("handle attach EOF");
        assert_eq!(end, Some(AttachStreamEnd::StreamClosed));
        output
            .restore(&mut stdout)
            .await
            .expect("restore terminal modes");

        assert_eq!(stdout, ATTACH_TERMINAL_MODE_CLEANUP);
        for disabled_mode in [
            b"\x1b[?1003l".as_slice(),
            b"\x1b[?1006l".as_slice(),
            b"\x1b[?1004l".as_slice(),
            b"\x1b[?2004l".as_slice(),
            b"\x1b[?1049l".as_slice(),
        ] {
            assert!(
                stdout
                    .windows(disabled_mode.len())
                    .any(|window| window == disabled_mode),
                "cleanup must contain {disabled_mode:?}"
            );
        }
    }

    #[tokio::test]
    async fn manual_detach_cleanup_follows_buffered_modal_output() {
        let mut stdout = Vec::new();
        let mut modal = ModalState {
            compositor: Compositor::new(80, 24),
            snapshot: AttachStatusSnapshot::unknown("local", "s-42"),
            cols: 80,
            pending_output: b"\x1b[?1003h\x1b[?1049hbuffered".to_vec(),
            active: true,
        };
        let mut output = TerminalOutputGuard::new(true);

        close_modal(&mut stdout, Some(&mut modal))
            .await
            .expect("replay modal output");
        output
            .restore(&mut stdout)
            .await
            .expect("restore after detach");

        assert!(
            stdout.ends_with(ATTACH_TERMINAL_MODE_CLEANUP),
            "terminal cleanup must follow buffered mode-enabling output"
        );
    }

    #[tokio::test]
    async fn non_tty_output_guard_emits_no_cleanup() {
        let mut stdout = Vec::new();
        let mut output = TerminalOutputGuard::new(false);

        output.restore(&mut stdout).await.expect("non-TTY no-op");

        assert!(stdout.is_empty());
    }

    #[tokio::test]
    async fn attach_modal_buffer_cap_resumes_passthrough_without_dropping_input() {
        let mut modal = Some(ModalState {
            compositor: Compositor::new(80, 24),
            snapshot: AttachStatusSnapshot::unknown("local", "s-42"),
            cols: 80,
            pending_output: Vec::new(),
            active: false,
        });
        let mut stdout = Vec::new();
        let mut menu = MenuRuntime::new(true);
        *menu.state.as_mut().expect("menu state") = MenuState::open_root();
        {
            let state = modal.as_mut().expect("modal state");
            state
                .compositor
                .set_overlay(MenuState::open_root().to_overlay_frame());
            open_modal(&mut stdout, Some(state))
                .await
                .expect("open modal");
            state.pending_output.resize(MAX_MODAL_BUFFER_BYTES, b'x');
        };
        stdout.clear();

        handle_socket_output(b"latest", &mut stdout, &mut modal, &mut menu)
            .await
            .expect("resume passthrough at buffer cap");

        assert!(stdout.ends_with(b"latest"));
        assert_eq!(menu.state, Some(MenuState::Closed));
        let state = modal.as_ref().expect("modal state");
        assert!(!state.active);
        assert!(state.pending_output.is_empty());
    }

    #[test]
    fn attach_banner_snapshot_updates_from_inspect_and_event_payloads() {
        let mut snapshot = AttachStatusSnapshot::unknown("local", "s-42");

        snapshot.update_from_session_value(&json!({
            "id": "s-42",
            "name": "review branch",
            "agent": "claude",
            "active_agent": "codex",
            "state": "running",
            "activity": "working",
            "project_id": "p-abc123",
            "project_label": "ui"
        }));
        assert!(
            render_banner_text(140, &snapshot)
                .contains("host=local  id=s-42  session=review branch  project=ui"),
            "banner text should include project and session name"
        );
        assert_eq!(snapshot.agent, "claude->codex");
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
                "agent": "shell",
                "active_agent": "claude",
                "state": "done"
            }
        }));
        assert_eq!(snapshot.agent, "shell->claude");
        assert_eq!(snapshot.state, "done");
        assert_eq!(snapshot.activity, "blocked");

        snapshot.update_from_event_value(&json!({
            "event": "agent_state",
            "session_id": "s-99",
            "activity": "idle"
        }));
        assert_eq!(snapshot.activity, "blocked");
    }

    #[test]
    fn attach_banner_snapshot_falls_back_to_ids_for_missing_display_labels() {
        let mut snapshot = AttachStatusSnapshot::unknown("local", "s-42");

        snapshot.update_from_session_value(&json!({
            "id": "s-42",
            "agent": "claude",
            "state": "running",
            "activity": "working",
            "project_id": "p-abc123"
        }));

        let text = render_banner_text(120, &snapshot);
        assert!(
            text.contains("host=local  id=s-42  session=s-42  project=p-abc123"),
            "banner text should fall back to ids when display labels are absent: {text:?}"
        );
    }
}
