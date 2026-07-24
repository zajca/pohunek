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

use pohunek_terminal::{
    step, Compositor, InputTranslator, MenuEffect, MenuEvent, MenuKey, MenuOutcome, MenuState,
};
use protocol::{
    event, method, ForkCwdMode, Request, SessionAttachParams, SessionAttachResult,
    SessionDetachParams, SessionForkParams, SessionForkResult, SessionId, SessionInfo,
    SessionNewParams, SessionNewResult, SessionRenameParams, SessionRenameResult,
    SessionResizeParams, SessionState, ENV_DAEMON_ID, ENV_SESSION_ID, ENV_WORKER_ID,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

use crate::client::{attach_raw, Client, RawStream};
use crate::commands::{request_id, request_with_params};
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::Target;

const DETACH_BYTE: u8 = 0x1d;
const IO_BUFFER_BYTES: usize = 8192;
// Default frame-coalescing cadence for the attach compositor.
//
// Agent output arrives in bursts of PTY chunks; rendering after every chunk
// wastes work, while too coarse a cadence makes the screen feel laggy. ~60 fps
// is imperceptible latency yet collapses a burst into a single repaint. Used
// when `banner_interval_seconds` is zero (the default); a positive config value
// overrides it to a coarser refresh.
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_millis(16);
// Config default: zero means "use DEFAULT_FRAME_INTERVAL" for the compositor.
const DEFAULT_BANNER_REPAINT_INTERVAL: Duration = Duration::ZERO;
// Two rows are not enough for a banner plus a usable agent viewport.
const MIN_ROWS_WITH_BANNER: u16 = 3;
const BANNER_MENU_LABEL: &str = "[menu:Ctrl-\\]";
// Two spaces keep dense banner fields readable in monospace terminals.
const BANNER_FIELD_SEPARATOR: &str = "  ";
// Ctrl-\ emits ASCII File Separator. Raw mode prevents the terminal driver from
// turning it into SIGQUIT, and the shortcut stays adjacent to Ctrl-] detach.
const BANNER_MENU_BYTE: u8 = 0x1c;
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
/// Poll interval while waiting for a daemon restart/resume after attach EOF.
///
/// A half-second cadence keeps reconnect responsive without turning a restart
/// outage into a tight socket-dial loop. Operators can override it in
/// `launcher.conf`.
const DEFAULT_ATTACH_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachReconnectConfig {
    window: Duration,
    interval: Duration,
}

impl Default for AttachReconnectConfig {
    fn default() -> Self {
        Self {
            window: DEFAULT_ATTACH_RECONNECT_WINDOW,
            interval: DEFAULT_ATTACH_RECONNECT_INTERVAL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachBannerConfig {
    enabled: bool,
    repaint_interval: Duration,
    reconnect: AttachReconnectConfig,
}

impl AttachBannerConfig {
    fn disabled() -> Self {
        Self {
            enabled: false,
            repaint_interval: DEFAULT_BANNER_REPAINT_INTERVAL,
            reconnect: AttachReconnectConfig::default(),
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
                    config.repaint_interval = parse_nonnegative_duration_seconds(
                        "banner_interval_seconds",
                        value.trim(),
                        &path,
                        number + 1,
                    )?;
                }
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
    name: String,
    project: String,
    agent: String,
    state: String,
    activity: String,
}

/// Live attach-banner state driving the [`Compositor`].
///
/// Holds the compositor that owns physical-terminal output, the latest session
/// snapshot rendered into the banner text, the terminal width used to lay that
/// text out, and the maximum repaint cadence.
#[derive(Debug)]
struct BannerState {
    compositor: Compositor,
    snapshot: AttachBannerSnapshot,
    cols: u16,
    frame_interval: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BannerInputAction {
    OpenMenu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BannerInputMatch {
    start: usize,
    action: BannerInputAction,
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
struct MenuEffectContext<'a> {
    client: &'a mut Client,
    stream_id: &'a str,
    terminal: &'a mut Option<RawTerminal>,
    menu_task: &'a mut Option<MenuTask>,
    generation: u64,
    control: &'a AttachControlContext,
    banner: Option<&'a BannerState>,
}

#[derive(Debug)]
struct StdinInputContext<'a, W> {
    socket_write: &'a mut W,
    client: &'a mut Client,
    stream_id: &'a str,
    terminal: &'a mut Option<RawTerminal>,
    banner: &'a mut Option<BannerState>,
    frame_deadline: &'a mut Option<Instant>,
    input_translator: &'a mut Option<InputTranslator>,
    menu: &'a mut MenuRuntime,
    control: &'a AttachControlContext,
}

impl AttachBannerSnapshot {
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
    let banner_config = AttachBannerConfig::load(paths)?;

    loop {
        let end = run_attach_once(
            host,
            paths,
            target,
            origin_session_id.clone(),
            origin_daemon_id.clone(),
            origin_worker_id.clone(),
            banner_config.clone(),
        )
        .await?;
        if end != AttachStreamEnd::StreamClosed || banner_config.reconnect.window.is_zero() {
            return Ok(());
        }
        if !wait_for_attach_reconnect(host, paths, target, &banner_config.reconnect).await? {
            return Ok(());
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
    banner_config: AttachBannerConfig,
) -> Result<AttachStreamEnd, CliError> {
    let attach_request = build_attach_request(
        target,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachStreamEnd {
    Detached,
    InputClosed,
    SessionStopped,
    StreamClosed,
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
) -> Result<AttachStreamEnd, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let terminal_size = terminal_size(libc::STDOUT_FILENO);
    let banner_terminal_size = banner_terminal_size(&banner_config, terminal_size);
    let banner_enabled = banner_terminal_size.is_some();

    if let Some((cols, rows)) =
        terminal_size.and_then(|size| effective_attach_size(size, banner_enabled))
    {
        if let Ok(request) = build_resize_request(target, cols, rows) {
            let _ = client.request(&request).await;
        }
    }

    let (banner, banner_updates) = if let Some((cols, rows)) = banner_terminal_size {
        let snapshot = load_initial_banner_snapshot(&mut client, host, &target.session_id).await;
        // Best-effort live snapshot updates: the banner reflects state/activity
        // changes while the subscription holds. If the update client cannot
        // connect or its stream drops, the banner keeps showing the last known
        // snapshot rather than tearing down the attach.
        let updates = spawn_banner_updates(
            host.to_owned(),
            paths.clone(),
            target.session_id.clone(),
            snapshot.clone(),
        );
        let state = BannerState {
            compositor: Compositor::new(cols, rows),
            snapshot,
            cols,
            frame_interval: effective_frame_interval(banner_config.repaint_interval),
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
        banner,
        banner_updates,
    )
    .await
}

/// Terminal size to render the banner at, or `None` when it should be skipped.
///
/// The banner needs an enabled config and a terminal tall enough to keep a
/// usable agent viewport below the reserved row.
fn banner_terminal_size(
    config: &AttachBannerConfig,
    terminal_size: Option<(u16, u16)>,
) -> Option<(u16, u16)> {
    if !config.enabled {
        return None;
    }
    terminal_size.filter(|&(_, rows)| rows >= MIN_ROWS_WITH_BANNER)
}

/// Resolves the compositor frame cadence from the configured repaint interval.
///
/// Zero (the default) selects [`DEFAULT_FRAME_INTERVAL`]; a positive value
/// throttles repaints to a coarser refresh.
fn effective_frame_interval(configured: Duration) -> Duration {
    if configured.is_zero() {
        DEFAULT_FRAME_INTERVAL
    } else {
        configured
    }
}

async fn load_initial_banner_snapshot(
    client: &mut Client,
    host: &str,
    session_id: &str,
) -> AttachBannerSnapshot {
    let mut snapshot = AttachBannerSnapshot::unknown(&banner_host_label(host), session_id);
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
    origin_session_id: Option<SessionId>,
    origin_daemon_id: Option<String>,
    origin_worker_id: Option<String>,
) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_ATTACH,
        &SessionAttachParams {
            session_id: SessionId(target.session_id.clone()),
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

fn render_banner_text(cols: u16, snapshot: &AttachBannerSnapshot) -> String {
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

fn translate_banner_input(
    input: &[u8],
    compositor: &Compositor,
    translator: &mut InputTranslator,
) -> Vec<u8> {
    translator.set_mouse_protocol(
        compositor.mouse_protocol_mode(),
        compositor.mouse_protocol_encoding(),
    );
    translator.push(input)
}

async fn write_banner_input<W>(
    writer: &mut W,
    input: &[u8],
    compositor: &Compositor,
    translator: &mut InputTranslator,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    let translated = translate_banner_input(input, compositor, translator);
    writer.write_all(&translated).await?;
    Ok(())
}

async fn write_stdin_input<W>(
    writer: &mut W,
    input: &[u8],
    banner: Option<&BannerState>,
    input_translator: &mut Option<InputTranslator>,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(banner) = banner {
        write_banner_input(
            writer,
            input,
            &banner.compositor,
            input_translator
                .as_mut()
                .expect("banner attach constructs an input translator"),
        )
        .await
    } else {
        writer.write_all(input).await?;
        Ok(())
    }
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

/// Renders one compositor frame from the current snapshot to `writer`.
async fn paint_banner<W>(writer: &mut W, banner: &mut BannerState) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    let text = render_banner_text(banner.cols, &banner.snapshot);
    let frame = banner.compositor.render(&text);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Restores the physical terminal on teardown when a banner is active.
async fn teardown_banner<W>(
    writer: &mut W,
    banner: Option<&mut BannerState>,
) -> Result<(), CliError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(banner) = banner {
        let frame = banner.compositor.reset();
        writer.write_all(&frame).await?;
        writer.flush().await?;
    }
    Ok(())
}

/// Awaits the next banner snapshot, or never resolves when updates are absent.
///
/// Kept as a named future so the `select!` arm stays a single line and the loop
/// body fits its line budget.
async fn recv_banner_update(
    updates: &mut Option<mpsc::UnboundedReceiver<AttachBannerSnapshot>>,
) -> Option<AttachBannerSnapshot> {
    match updates.as_mut() {
        Some(updates) => updates.recv().await,
        None => None,
    }
}

/// Waits until the pending frame deadline, or forever when none is scheduled.
///
/// Lets the select loop coalesce a burst of agent output into a single repaint:
/// the first byte arms a deadline, and this branch fires once it elapses.
async fn wait_for_frame_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending().await,
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

fn arm_frame_deadline(banner: &BannerState, frame_deadline: &mut Option<Instant>) {
    if frame_deadline.is_none() {
        *frame_deadline = Some(Instant::now() + banner.frame_interval);
    }
}

fn sync_menu_overlay(
    menu_state: &MenuState,
    banner: &mut Option<BannerState>,
    frame_deadline: &mut Option<Instant>,
) {
    if let Some(banner) = banner.as_mut() {
        banner.compositor.set_overlay(menu_state.to_overlay_frame());
        arm_frame_deadline(banner, frame_deadline);
    }
}

fn next_menu_generation(generation: u64) -> u64 {
    generation.wrapping_add(MENU_GENERATION_STEP)
}

async fn handle_socket_output<W>(
    input: &[u8],
    stdout: &mut W,
    banner: &mut Option<BannerState>,
    frame_deadline: &mut Option<Instant>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
{
    if input.is_empty() {
        return Ok(Some(AttachStreamEnd::StreamClosed));
    }
    if let Some(banner) = banner.as_mut() {
        banner.compositor.feed(input);
        if frame_deadline.is_none() {
            *frame_deadline = Some(Instant::now() + banner.frame_interval);
        }
    } else {
        stdout.write_all(input).await?;
        stdout.flush().await?;
    }
    Ok(None)
}

fn handle_banner_update(
    update: Option<AttachBannerSnapshot>,
    banner: &mut Option<BannerState>,
    banner_updates: &mut Option<mpsc::UnboundedReceiver<AttachBannerSnapshot>>,
    frame_deadline: &mut Option<Instant>,
) {
    if let Some(snapshot) = update {
        if let Some(banner) = banner.as_mut() {
            banner.snapshot = snapshot;
            if frame_deadline.is_none() {
                *frame_deadline = Some(Instant::now() + banner.frame_interval);
            }
        }
    } else {
        *banner_updates = None;
    }
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

fn config_error(message: String) -> CliError {
    CliError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// Applies a window-size change to the compositor and the daemon PTY.
///
/// Reads the current terminal size, resizes the banner compositor (which forces
/// a full repaint), and sends the daemon the effective agent-grid size. Returns
/// whether a banner is active, so the caller can schedule an immediate repaint.
async fn apply_terminal_resize(
    client: &mut Client,
    target: &Target,
    banner: &mut Option<BannerState>,
) -> bool {
    let Some((cols, rows)) = terminal_size(libc::STDOUT_FILENO) else {
        return false;
    };
    if let Some(banner) = banner.as_mut() {
        banner.cols = cols;
        banner.compositor.resize(cols, rows);
    }
    let effective_size =
        effective_attach_size((cols, rows), banner.is_some()).unwrap_or((cols, rows));
    if let Ok(request) = build_resize_request(target, effective_size.0, effective_size.1) {
        let _ = client.request(&request).await;
    }
    banner.is_some()
}

async fn handle_stdin_input<W>(
    input: &[u8],
    ctx: StdinInputContext<'_, W>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(end) = handle_detach_input(
        input,
        ctx.socket_write,
        ctx.client,
        ctx.stream_id,
        ctx.terminal,
    )
    .await?
    {
        return Ok(Some(end));
    }

    if let Some(state) = ctx
        .menu
        .state
        .as_mut()
        .filter(|state| **state != MenuState::Closed)
    {
        let outcome = handle_menu_input_chunk_with_decoder(state, &mut ctx.menu.decoder, input);
        if outcome.changed {
            sync_menu_overlay(state, ctx.banner, ctx.frame_deadline);
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
                banner: ctx.banner.as_ref(),
            },
        )
        .await;
    }

    if let Some(action) =
        handle_banner_input_action(input, ctx.socket_write, ctx.banner.is_some()).await?
    {
        match action {
            BannerInputAction::OpenMenu => {
                ctx.menu.generation = next_menu_generation(ctx.menu.generation);
                if let Some(state) = ctx.menu.state.as_mut() {
                    *state = MenuState::open_root();
                    sync_menu_overlay(state, ctx.banner, ctx.frame_deadline);
                }
            }
        }
        return Ok(None);
    }

    write_stdin_input(
        ctx.socket_write,
        input,
        ctx.banner.as_ref(),
        ctx.input_translator,
    )
    .await?;
    ctx.socket_write.flush().await?;
    Ok(None)
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
    mut banner: Option<BannerState>,
    mut banner_updates: Option<mpsc::UnboundedReceiver<AttachBannerSnapshot>>,
) -> Result<AttachStreamEnd, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut terminal = RawTerminal::enable(libc::STDIN_FILENO)?;
    let (mut socket_read, mut socket_write) = tokio::io::split(stream);
    let mut winch = signal(SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buf = [0_u8; IO_BUFFER_BYTES];
    let mut socket_buf = [0_u8; IO_BUFFER_BYTES];
    let mut input_translator = banner.as_ref().map(|_| InputTranslator::new());
    let mut menu = MenuRuntime::new(banner.is_some());
    let mut frame_deadline: Option<Instant> = None;

    let outcome: Result<AttachStreamEnd, CliError> = async {
        if let Some(banner) = banner.as_mut() {
            paint_banner(&mut stdout, banner).await?;
        }

        loop {
            tokio::select! {
                read = socket_read.read(&mut socket_buf) => {
                    let bytes_read = read?;
                    if let Some(end) = handle_socket_output(
                        &socket_buf[..bytes_read],
                        &mut stdout,
                        &mut banner,
                        &mut frame_deadline,
                    )
                    .await?
                    {
                        return Ok(end);
                    }
                }
                read = stdin.read(&mut stdin_buf) => {
                    let bytes_read = read?;
                    if bytes_read == 0 {
                        socket_write.shutdown().await?;
                        return Ok(AttachStreamEnd::InputClosed);
                    }
                    if let Some(end) = handle_stdin_input(
                        &stdin_buf[..bytes_read],
                        StdinInputContext {
                            socket_write: &mut socket_write,
                            client: &mut client,
                            stream_id: &stream_id,
                            terminal: &mut terminal,
                            banner: &mut banner,
                            frame_deadline: &mut frame_deadline,
                            input_translator: &mut input_translator,
                            menu: &mut menu,
                            control: &control,
                        },
                    )
                    .await?
                    {
                        return Ok(end);
                    }
                }
                resized = winch.recv() => {
                    if resized.is_none() {
                        continue;
                    }
                    // Repaint at once so the resized grid and region are correct.
                    if apply_terminal_resize(&mut client, &control.target, &mut banner).await {
                        frame_deadline = Some(Instant::now());
                    }
                }
                update = recv_banner_update(&mut banner_updates), if banner_updates.is_some() => {
                    handle_banner_update(
                        update,
                        &mut banner,
                        &mut banner_updates,
                        &mut frame_deadline,
                    );
                }
                () = wait_for_frame_deadline(frame_deadline), if frame_deadline.is_some() => {
                    frame_deadline = None;
                    if let Some(banner) = banner.as_mut() {
                        paint_banner(&mut stdout, banner).await?;
                    }
                }
                task = wait_for_menu_task(&mut menu.task), if menu.task.is_some() => {
                    if let Some(end) = handle_menu_task_result(
                        task,
                        &control.target,
                        &mut menu.task,
                        &mut menu.state,
                        &mut banner,
                        &mut frame_deadline,
                        &mut terminal,
                    ) {
                        return Ok(end);
                    }
                }
            }
        }
    }
    .await;

    // Best-effort restore; do not mask the loop's real outcome with a teardown
    // write error.
    let _ = teardown_banner(&mut stdout, banner.as_mut()).await;
    outcome
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
) -> Result<bool, CliError> {
    let deadline = time::Instant::now() + config.window;
    eprintln!(
        "[pohunek] attach stream closed; waiting up to {:.1}s for session {} to resume",
        config.window.as_secs_f64(),
        target.session_id
    );

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
                "[pohunek] session {} did not resume before reconnect timeout",
                target.session_id
            );
            return Ok(false);
        }
        time::sleep(std::cmp::min(config.interval, deadline - now)).await;
    }
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
                if let (None, Some(banner)) = (ctx.menu_task.as_ref(), ctx.banner) {
                    let (rows, cols) = banner.compositor.grid_size();
                    *ctx.menu_task = Some(spawn_menu_task(
                        ctx.generation,
                        MenuTaskAction::NewSession,
                        ctx.control,
                        MenuTaskArgs::NewSession { cols, rows },
                    ));
                }
            }
            MenuEffect::RunFork => {
                if let (None, Some(banner)) = (ctx.menu_task.as_ref(), ctx.banner) {
                    let (rows, cols) = banner.compositor.grid_size();
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

fn handle_menu_task_result(
    result: Option<MenuTaskResult>,
    target: &Target,
    menu_task: &mut Option<MenuTask>,
    menu_state: &mut Option<MenuState>,
    banner: &mut Option<BannerState>,
    frame_deadline: &mut Option<Instant>,
    terminal: &mut Option<RawTerminal>,
) -> Option<AttachStreamEnd> {
    let result = result?;
    *menu_task = None;
    if result.action == MenuTaskAction::Kill && result.outcome.is_ok() {
        terminal.take();
        return Some(AttachStreamEnd::SessionStopped);
    }

    let Some(state) = menu_state.as_mut() else {
        log_abandoned_menu_task_result(target, &result);
        return None;
    };
    if *state == MenuState::Closed {
        log_abandoned_menu_task_result(target, &result);
        return None;
    }

    let event = menu_event_from_task_outcome(result.outcome);
    let before = state.clone();
    let (next, effects) = step(before.clone(), event);
    *state = next;
    if *state != before {
        sync_menu_overlay(state, banner, frame_deadline);
    }
    debug_assert!(
        effects.is_empty(),
        "RPC completion events must not request new menu effects"
    );
    None
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

async fn send_detach(client: &mut Client, stream_id: &str) -> Result<(), CliError> {
    let request = build_detach_request(stream_id)?;
    let _ = client.request(&request).await?;
    Ok(())
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

async fn handle_detach_input<W>(
    input: &[u8],
    socket_write: &mut W,
    client: &mut Client,
    stream_id: &str,
    terminal: &mut Option<RawTerminal>,
) -> Result<Option<AttachStreamEnd>, CliError>
where
    W: AsyncWrite + Unpin,
{
    let Some(detach_at) = input.iter().position(|byte| *byte == DETACH_BYTE) else {
        return Ok(None);
    };
    if detach_at > 0 {
        socket_write.write_all(&input[..detach_at]).await?;
        socket_write.flush().await?;
    }
    terminal.take();
    let _ = send_detach(client, stream_id).await;
    Ok(Some(AttachStreamEnd::Detached))
}

async fn handle_banner_input_action<W>(
    input: &[u8],
    socket_write: &mut W,
    banner_active: bool,
) -> Result<Option<BannerInputAction>, CliError>
where
    W: AsyncWrite + Unpin,
{
    // The menu shortcut only exists while the banner is shown.
    if !banner_active {
        return Ok(None);
    }
    let Some(input_match) = find_banner_input_action(input) else {
        return Ok(None);
    };

    if input_match.start > 0 {
        socket_write.write_all(&input[..input_match.start]).await?;
        socket_write.flush().await?;
    }
    Ok(Some(input_match.action))
}

async fn send_stop(client: &mut Client, target: &Target) -> Result<(), CliError> {
    let request = build_stop_request(target)?;
    let _ = client.request(&request).await?;
    Ok(())
}

fn build_stop_request(target: &Target) -> Result<Request, CliError> {
    request_with_params(method::SESSION_STOP, &SessionId(target.session_id.clone()))
}

fn parse_banner_input_action(input: &[u8]) -> Option<BannerInputAction> {
    input
        .contains(&BANNER_MENU_BYTE)
        .then_some(BannerInputAction::OpenMenu)
}

fn find_banner_input_action(input: &[u8]) -> Option<BannerInputMatch> {
    let start = input.iter().position(|byte| *byte == BANNER_MENU_BYTE)?;
    let action = parse_banner_input_action(&input[start..=start])?;
    Some(BannerInputMatch { start, action })
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
        // The id is now a unique per-call SDK correlation id; assert only its
        // stable, log-greppable `sdk-<method>-` prefix.
        assert!(
            request.id.starts_with(&format!("sdk-{method_name}-")),
            "id {:?} must be prefixed by the method",
            request.id
        );
    }

    #[test]
    fn attach_request_sends_session_id() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_attach_request(&target, None, None, None).expect("request");

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
            Some("worker-xyz".to_owned()),
        )
        .expect("request");

        assert_request(
            &request,
            method::SESSION_ATTACH,
            json!({
                "session_id": "s-42",
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
        let request = build_attach_request(&remote, None, None, None).expect("request");

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
            "banner=true\n\
             banner_interval_seconds=0.25\n\
             attach_reconnect_seconds=12\n\
             attach_reconnect_interval_seconds=0.75\n",
        )
        .expect("write config");

        let config = AttachBannerConfig::load_from_config_dir(&root).expect("load config");

        assert!(config.enabled, "banner=true should request attach banner");
        assert_eq!(
            config.repaint_interval,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(config.reconnect.window, std::time::Duration::from_secs(12));
        assert_eq!(
            config.reconnect.interval,
            std::time::Duration::from_millis(750)
        );
    }

    #[test]
    fn attach_banner_config_disables_idle_repaint_by_default() {
        let root = std::env::temp_dir().join(format!(
            "pohunek-attach-banner-default-repaint-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create config dir");
        std::fs::write(root.join("launcher.conf"), "banner=true\n").expect("write config");

        let config = AttachBannerConfig::load_from_config_dir(&root).expect("load config");

        assert!(config.enabled, "banner=true should request attach banner");
        assert_eq!(
            config.repaint_interval,
            std::time::Duration::ZERO,
            "banner should draw initially without automatic repainting by default"
        );
    }

    #[test]
    fn banner_terminal_size_requires_enabled_config_and_room_for_a_viewport() {
        let enabled = AttachBannerConfig {
            enabled: true,
            repaint_interval: std::time::Duration::ZERO,
            reconnect: AttachReconnectConfig::default(),
        };
        let disabled = AttachBannerConfig::disabled();

        assert_eq!(
            banner_terminal_size(&enabled, Some((120, 40))),
            Some((120, 40)),
            "an enabled banner uses the full terminal size"
        );
        assert_eq!(
            banner_terminal_size(&enabled, Some((120, 2))),
            None,
            "a terminal too short for a usable viewport skips the banner"
        );
        assert_eq!(
            banner_terminal_size(&enabled, None),
            None,
            "a missing terminal size skips the banner"
        );
        assert_eq!(
            banner_terminal_size(&disabled, Some((120, 40))),
            None,
            "a disabled config never shows the banner"
        );
    }

    #[test]
    fn effective_frame_interval_defaults_zero_to_the_frame_cadence() {
        assert_eq!(
            effective_frame_interval(std::time::Duration::ZERO),
            DEFAULT_FRAME_INTERVAL,
            "zero selects the built-in frame cadence"
        );
        assert_eq!(
            effective_frame_interval(std::time::Duration::from_millis(200)),
            std::time::Duration::from_millis(200),
            "a positive value throttles repaints to a coarser refresh"
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
    fn attach_banner_reserves_one_terminal_row_for_daemon_resize() {
        assert_eq!(effective_attach_size((120, 40), true), Some((120, 39)));
        assert_eq!(effective_attach_size((120, 40), false), Some((120, 40)));
        assert_eq!(effective_attach_size((120, 2), true), None);
    }

    #[test]
    fn attach_banner_text_prioritizes_live_state_and_menu_shortcut() {
        let mut snapshot = AttachBannerSnapshot::unknown("local", "s-42");
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
        let mut snapshot = AttachBannerSnapshot::unknown("local", "s-37");
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
    fn attach_banner_menu_shortcut_is_parsed_from_input() {
        assert_eq!(
            parse_banner_input_action(&[BANNER_MENU_BYTE]),
            Some(BannerInputAction::OpenMenu),
            "the Ctrl-\\ byte must map to the open-menu action"
        );
    }

    #[test]
    fn attach_banner_menu_trigger_splits_prefix_and_drops_suffix() {
        let input = b"echo prefix\x1ctail dropped";
        let input_match = find_banner_input_action(input).expect("menu trigger");

        assert_eq!(input_match.start, "echo prefix".len());
        assert_eq!(input_match.action, BannerInputAction::OpenMenu);
    }

    #[test]
    fn attach_banner_non_menu_input_is_forwarded() {
        let input = b"\x1b[<64;7;1M";

        assert_eq!(parse_banner_input_action(input), None);
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
        let mut banner = None;
        let mut frame_deadline = None;
        let mut terminal = None;
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
            &mut banner,
            &mut frame_deadline,
            &mut terminal,
        );

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
        let mut banner = None;
        let mut frame_deadline = None;
        let mut terminal = None;
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
            &mut banner,
            &mut frame_deadline,
            &mut terminal,
        );

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
        let mut banner = None;
        let mut frame_deadline = None;
        let mut terminal = None;
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
            &mut banner,
            &mut frame_deadline,
            &mut terminal,
        );

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

    #[test]
    fn attach_banner_mouse_input_is_translated_to_agent_grid() {
        let mut compositor = Compositor::new(80, 24);
        compositor.feed(b"\x1b[?1000h\x1b[?1006h");
        let mut translator = InputTranslator::new();

        assert_eq!(
            translate_banner_input(b"\x1b[<64;7;2M", &compositor, &mut translator),
            b"\x1b[<64;7;1M"
        );
        assert_eq!(
            translate_banner_input(b"\x1b[<64;7;1M", &compositor, &mut translator),
            b""
        );
    }

    #[test]
    fn attach_banner_snapshot_updates_from_inspect_and_event_payloads() {
        let mut snapshot = AttachBannerSnapshot::unknown("local", "s-42");

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
        let mut snapshot = AttachBannerSnapshot::unknown("local", "s-42");

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
