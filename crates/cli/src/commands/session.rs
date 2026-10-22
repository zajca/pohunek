//! `pohunek session` — manage PTY-backed sessions on a local or remote host.
//!
//! The CLI grammar is host-aware through [`crate::target::Target`]; the effective
//! host selects the transport ([`Client`] dials the local Unix socket or a remote
//! `NetBird` TCP connection). The session-id is the same on either side, so the
//! request-building functions are transport-agnostic. Starting a session on a
//! *remote* host goes through a confirmation gate (see [`confirmation_decision`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
#[cfg(test)]
use protocol::Request;
use protocol::{
    method, AgentActivity, CwdSource, ForkCwdMode, OutputOffset, RuntimeGeneration,
    SessionDetectionParams, SessionDetectionResult, SessionDiffParams, SessionForkParams,
    SessionId, SessionIdleStopParams, SessionIdleStopResult, SessionInfo, SessionInputParams,
    SessionInputResult, SessionInputWait, SessionListFilter, SessionListParams, SessionNewParams,
    SessionOutputParams, SessionPolicyParams, SessionReadFormat, SessionReadParams,
    SessionReadResult, SessionReadSource, SessionRemoveResult, SessionRenameParams,
    SessionResizeParams, SessionResumeParams, SessionRetentionParams, SessionRetentionPolicy,
    SessionRetentionResult, SessionRuntimeIdentity, SessionScreenParams, SessionSetMetadataParams,
    SessionState, SessionStopResult, SessionTurnCursor, SessionTurnEventKind, SessionTurnProgress,
    SessionWaitParams, SessionWaitTurnParams, SessionWaitTurnReason, SessionWaitTurnResult,
    SessionWarningKind, StateSource, SubagentLifecycle, TerminalWatermark, MAX_SESSION_INPUT_BYTES,
    MAX_SESSION_OUTPUT_BYTES, MAX_SESSION_READ_LINES, MAX_SESSION_WAIT_TURN_MS,
};

use crate::client::{Client, PinnedHost};
#[cfg(test)]
use crate::commands::request_with_params;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{is_local_host, Target};

/// How long a resume, fork, stop or remove runs before the CLI tells the
/// operator it is still waiting for the daemon (the ordinary request timeout
/// of other commands).
const LIFECYCLE_PROGRESS_NOTICE_AFTER: Duration = Duration::from_secs(5);

/// Default fork PTY width when the command is not launched from an attach view.
///
/// Matches `session new`'s CLI default so non-interactive fork callers get the
/// same baseline terminal geometry unless a richer surface supplies live size.
const DEFAULT_FORK_COLS: u16 = 80;
/// Default fork PTY height when the command is not launched from an attach view.
///
/// Matches `session new`'s CLI default; attach and other interactive clients send live size.
const DEFAULT_FORK_ROWS: u16 = 24;

/// Default observation page balances useful output with compact CLI responses.
pub(crate) const DEFAULT_OUTPUT_BYTES: u32 = 16 * 1024;

/// Parse an output page size while enforcing the public protocol ceiling.
pub(crate) fn parse_output_bytes(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| "--max-bytes must be an unsigned 32-bit integer".to_owned())?;
    if parsed == 0 || usize::try_from(parsed).unwrap_or(usize::MAX) > MAX_SESSION_OUTPUT_BYTES {
        return Err(format!(
            "--max-bytes must be between 1 and {MAX_SESSION_OUTPUT_BYTES}"
        ));
    }
    Ok(parsed)
}

/// Parse a required bounded wait timeout against the shared protocol ceiling.
pub(crate) fn parse_wait_timeout_ms(value: &str) -> Result<u32, String> {
    parse_bounded_wait_timeout_ms(value, "--timeout-ms")
}

/// Parse an input-wait timeout against the shared protocol ceiling.
pub(crate) fn parse_input_timeout_ms(value: &str) -> Result<u32, String> {
    parse_bounded_wait_timeout_ms(value, "--timeout")
}

fn parse_bounded_wait_timeout_ms(value: &str, flag: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| format!("{flag} must be an unsigned 32-bit integer"))?;
    if parsed == 0 || parsed > protocol::MAX_SESSION_WAIT_MS {
        return Err(format!(
            "{flag} must be between 1 and {}",
            protocol::MAX_SESSION_WAIT_MS
        ));
    }
    Ok(parsed)
}

/// Turn event kind accepted by `session wait --until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub(crate) enum TurnKindArg {
    /// The agent finished a turn.
    TurnCompleted,
    /// The agent is blocked on the owner.
    AgentBlocked,
    /// The agent waits for an approval decision.
    ApprovalRequired,
    /// The session entered a terminal lifecycle state.
    Exited,
}

impl From<TurnKindArg> for SessionTurnEventKind {
    fn from(kind: TurnKindArg) -> Self {
        match kind {
            TurnKindArg::TurnCompleted => Self::TurnCompleted,
            TurnKindArg::AgentBlocked => Self::AgentBlocked,
            TurnKindArg::ApprovalRequired => Self::ApprovalRequired,
            TurnKindArg::Exited => Self::Exited,
        }
    }
}

/// Turn event kinds a `session wait` without `--until` selects.
///
/// Every normal end of a turn is included: a wait that leaves one out times
/// out when the agent ends its turn that way.
const ALL_TURN_KINDS: [SessionTurnEventKind; 4] = [
    SessionTurnEventKind::TurnCompleted,
    SessionTurnEventKind::AgentBlocked,
    SessionTurnEventKind::ApprovalRequired,
    SessionTurnEventKind::Exited,
];

/// Milliseconds per unit accepted by [`parse_turn_timeout`].
const TURN_TIMEOUT_UNITS: [(&str, u64); 4] = [
    ("ms", 1),
    ("s", 1_000),
    ("m", 60 * 1_000),
    ("h", 60 * 60 * 1_000),
];

/// Parse a `session wait --timeout` duration such as `500ms`, `90s`, `5m`, or
/// `1h` into milliseconds.
///
/// A unit is required: `session input --timeout` takes bare milliseconds, so
/// a bare number would mean different things on sibling flags. The upper bound
/// is the protocol's absolute turn-wait ceiling; the daemon rejects a value
/// above its own configured ceiling with `session_wait_limit_exceeded`.
pub(crate) fn parse_turn_timeout(value: &str) -> Result<u32, String> {
    let invalid = || {
        format!(
            "--timeout must be a positive integer followed by ms, s, m, or h \
             (for example 90s or 5m), got {value:?}"
        )
    };
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (number, unit) = value.split_at(split);
    let scale = TURN_TIMEOUT_UNITS
        .iter()
        .find_map(|(name, scale)| (*name == unit).then_some(*scale))
        .ok_or_else(invalid)?;
    let count = number.parse::<u64>().map_err(|_error| invalid())?;
    let milliseconds = count.saturating_mul(scale);
    if milliseconds == 0 {
        return Err("--timeout must be greater than zero".to_owned());
    }
    u32::try_from(milliseconds)
        .ok()
        .filter(|milliseconds| *milliseconds <= MAX_SESSION_WAIT_TURN_MS)
        .ok_or_else(|| format!("--timeout must not exceed {MAX_SESSION_WAIT_TURN_MS} ms (24h)"))
}

/// Parse a `session wait --after` cursor with the protocol's cursor checks.
pub(crate) fn parse_turn_cursor(value: &str) -> Result<SessionTurnCursor, String> {
    SessionTurnCursor::new(value).map_err(|_error| {
        format!(
            "--after must be the cursor of an earlier session wait: 1 to {} printable ASCII bytes",
            protocol::MAX_SESSION_TURN_CURSOR_BYTES
        )
    })
}

/// Parse an input acknowledgement window of `session new`.
pub(crate) fn parse_input_ack_timeout_ms(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| "--input-ack-timeout-ms must be an unsigned 32-bit integer".to_owned())?;
    if !(1..=protocol::MAX_SESSION_WAIT_MS).contains(&parsed) {
        return Err(format!(
            "--input-ack-timeout-ms must be between 1 and {} milliseconds",
            protocol::MAX_SESSION_WAIT_MS
        ));
    }
    Ok(parsed)
}

/// Parse a non-zero daemon response timeout for `session new`.
pub(crate) fn parse_request_timeout_ms(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| "--request-timeout-ms must be an unsigned 32-bit integer".to_owned())?;
    if parsed == 0 {
        return Err("--request-timeout-ms must be greater than zero".to_owned());
    }
    Ok(parsed)
}

/// Maximum text accepted from stdin for `session new` and `session input`.
///
/// The daemon remains authoritative for provider-specific safe-text policy.
pub(crate) const MAX_STDIN_INPUT_BYTES: usize = MAX_SESSION_INPUT_BYTES;

/// Read and validate bounded UTF-8 text from a reader.
///
/// Newlines, carriage returns, and tabs are intentional terminal text. Other
/// control characters are rejected before a request is constructed, and the
/// diagnostic never contains the supplied payload.
pub(crate) fn read_bounded_input<R: std::io::Read>(reader: R) -> Result<String, CliError> {
    let read_limit =
        u64::try_from(MAX_STDIN_INPUT_BYTES + 1).expect("stdin limit plus sentinel fits u64");
    let mut bytes = Vec::with_capacity(MAX_STDIN_INPUT_BYTES.min(8 * 1024));
    reader.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STDIN_INPUT_BYTES {
        return Err(CliError::InvalidStdinInput {
            detail: format!("input exceeds the maximum of {MAX_STDIN_INPUT_BYTES} UTF-8 bytes"),
        });
    }
    let input = String::from_utf8(bytes).map_err(|_error| CliError::InvalidStdinInput {
        detail: "input is not valid UTF-8".to_owned(),
    })?;
    if input
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(CliError::InvalidStdinInput {
            detail: "input contains a disallowed control character".to_owned(),
        });
    }
    Ok(input)
}

/// Read one bounded terminal text payload from process stdin.
pub(crate) fn read_stdin_input() -> Result<String, CliError> {
    read_bounded_input(std::io::stdin().lock())
}

/// Resolve an exact session display name to its unique stable id on the host an
/// operation pinned for all its connections.
///
/// Exact ids always win. Unknown values are left unchanged so the daemon emits
/// its canonical not-found error. Ambiguity is rejected locally with sorted id
/// candidates, which makes retries deterministic across list ordering.
pub(crate) async fn resolve_target_pinned(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
) -> Result<Target, CliError> {
    let client = Client::connect_pinned(host, paths).await?;
    resolve_target_on(client, target).await
}

async fn resolve_target_on(client: Client, target: &Target) -> Result<Target, CliError> {
    let mut sdk = client.into_sdk();
    let sessions = sdk
        .call::<protocol::method::SessionList>(SessionListParams {
            filters: Vec::new(),
        })
        .await?;
    resolve_target_from_sessions(target, &sessions)
}

fn resolve_target_from_sessions(
    target: &Target,
    sessions: &[SessionInfo],
) -> Result<Target, CliError> {
    if sessions
        .iter()
        .any(|session| session.id.0 == target.session_id)
    {
        return Ok(target.clone());
    }
    let mut candidates = sessions
        .iter()
        .filter(|session| session.name.as_deref() == Some(target.session_id.as_str()))
        .map(|session| session.id.0.clone())
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    match candidates.as_slice() {
        [] => Ok(target.clone()),
        [session_id] => Ok(Target {
            host: target.host.clone(),
            session_id: session_id.clone(),
        }),
        _ => Err(CliError::AmbiguousSessionName {
            candidates: candidates.join(", "),
        }),
    }
}

/// Process-wide cancellation signals (SIGINT and SIGTERM on Unix, Ctrl-C elsewhere).
///
/// [`Self::install`] registers the handlers immediately, so a signal that arrives
/// before [`Self::cancelled`] is first polled is queued instead of taking the
/// default action. Install it before sending a request whose outcome the signal
/// must be reported against.
#[cfg(unix)]
struct ProcessCancellation {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ProcessCancellation {
    fn install() -> Result<Self, CliError> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    /// Completes once a cancellation signal has been delivered since installation.
    async fn cancelled(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

/// Process-wide cancellation signals (Ctrl-C only on this platform).
#[cfg(not(unix))]
struct ProcessCancellation;

#[cfg(not(unix))]
impl ProcessCancellation {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "matches the Unix constructor, which can fail to register a handler"
    )]
    fn install() -> Result<Self, CliError> {
        Ok(Self)
    }

    /// Completes once Ctrl-C has been delivered.
    async fn cancelled(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn cancellable<T>(
    future: impl std::future::Future<Output = Result<T, pohunek_client::ClientError>>,
) -> Result<T, CliError> {
    let mut cancellation = ProcessCancellation::install()?;
    tokio::select! {
        result = future => result.map_err(CliError::from),
        () = cancellation.cancelled() => Err(CliError::Cancelled),
    }
}

/// Arguments for `session new`, grouped to keep the call site readable as the
/// optional worktree flags accumulate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewArgs {
    /// Agent NAME to start: an installed runtime (the built-in `shell`, or a package runtime such as `codex`, `claude` or `hermes`) or a host
    /// profile, resolved daemon-side on the target host (Part C, free string).
    pub agent: String,
    /// Owner-set display name for the session, or `None` to show it by id.
    pub name: Option<String>,
    /// Directory the session starts in; refused together with a worktree branch.
    pub cwd: Option<PathBuf>,
    /// Initial terminal columns.
    pub cols: u16,
    /// Initial terminal rows.
    pub rows: u16,
    /// Project to run in, by `<id|label>` (resolved daemon-side).
    pub project: Option<String>,
    /// Repository to bind a worktree for / run in-place / register.
    pub repo: Option<PathBuf>,
    /// Branch to check out in the worktree.
    pub branch: Option<String>,
    /// Base branch the worktree's branch is created from.
    pub base_branch: Option<String>,
    /// Start the worktree from `origin/<branch>` (`--from-remote`).
    pub from_remote: bool,
    /// Keep a diverged local branch as a backup and reset it (`--reset-diverged`).
    pub reset_diverged: bool,
    /// Initial text to inject into the spawned PTY.
    pub input: Option<String>,
    /// Acknowledgement window for the initial input; the result then reports
    /// whether the agent started working on it.
    pub input_ack_timeout_ms: Option<u32>,
    /// Optional daemon response timeout for this creation request.
    pub request_timeout_ms: Option<u32>,
    /// Parsed `--meta KEY=VALUE` pairs, one per repeated flag. Shape-validated
    /// per pair by [`parse_meta_pair`] (the clap value parser); duplicate-key
    /// rejection happens once the full set is known, in [`parse_meta_pairs`].
    pub meta: Vec<(String, String)>,
}

/// The decision the confirmation gate makes before a `session new` connects.
///
/// Factored out as a pure function ([`confirmation_decision`]) so the policy is
/// unit-testable without a daemon or a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmDecision {
    /// Start the session without confirmation (local, or remote with `--yes`).
    Proceed,
    /// Remote on the machine path (`--json`) and no `--yes`: fail fast, do not
    /// silently block on an interactive prompt.
    RequireYes,
    /// Remote on the human path and no `--yes`: prompt interactively.
    Prompt,
}

/// Output mode for `session list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListOutputMode {
    /// Human-readable table.
    Human,
    /// Stable machine-readable JSON.
    Json,
    /// One session id per line.
    Quiet,
}

/// A single exact-match `session list --filter key=value` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListFilter {
    /// Match the session id.
    Id(String),
    /// Match the session lifecycle state.
    State(SessionState),
    /// Match the detected activity.
    Activity(AgentActivity),
    /// Match the agent name (a base kind or a host profile). The daemon groups by
    /// the snapshotted base kind too; this client-side check is exact-name.
    Agent(String),
    /// Match the session's project by `<id|label>` reference.
    Project(String),
    /// Match an opaque metadata key and its exact value.
    MetadataEq { key: String, value: String },
    /// Match the presence of an opaque metadata key.
    MetadataHas { key: String },
}

impl ListFilter {
    /// Whether this filter matches one session exactly.
    #[must_use]
    pub(crate) fn matches(&self, session: &SessionInfo) -> bool {
        self.to_protocol_filter().matches(session)
    }

    fn to_protocol_filter(&self) -> SessionListFilter {
        match self {
            ListFilter::Id(id) => SessionListFilter::Id(id.clone()),
            ListFilter::State(state) => SessionListFilter::State(*state),
            ListFilter::Activity(activity) => SessionListFilter::Activity(*activity),
            ListFilter::Agent(agent) => SessionListFilter::Agent(agent.clone()),
            ListFilter::Project(reference) => SessionListFilter::Project(reference.clone()),
            ListFilter::MetadataEq { key, value } => SessionListFilter::MetadataEq {
                key: key.clone(),
                value: value.clone(),
            },
            ListFilter::MetadataHas { key } => SessionListFilter::MetadataHas { key: key.clone() },
        }
    }
}

/// Parse one `session list --meta-eq key=value` argument.
///
/// Metadata keys and values are opaque, so only the separator is required;
/// unlike the legacy named filters, either side may be empty.
pub(crate) fn parse_metadata_eq_filter(input: &str) -> Result<(String, String), String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| format!("invalid metadata equality filter {input:?}: expected key=value"))?;
    Ok((key.to_owned(), value.to_owned()))
}

/// Parse one `session list --filter key=value` argument.
///
/// The parser is intentionally owned by the session command, but clap uses it as
/// a value parser so invalid filters go through the existing usage-error sink.
pub(crate) fn parse_list_filter(input: &str) -> Result<ListFilter, String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| format!("invalid filter {input:?}: expected key=value"))?;
    if key.is_empty() {
        return Err(format!(
            "invalid filter {input:?}: filter key cannot be empty"
        ));
    }
    if value.is_empty() {
        return Err(format!(
            "invalid filter {input:?}: filter value cannot be empty"
        ));
    }

    match key {
        "id" => Ok(ListFilter::Id(value.to_owned())),
        "state" => parse_state_filter(value).map(ListFilter::State),
        "activity" => parse_activity_filter(value).map(ListFilter::Activity),
        "agent" => parse_agent_filter(value).map(ListFilter::Agent),
        "project" => Ok(ListFilter::Project(value.to_owned())),
        other => Err(format!(
            "unknown filter key {other:?}; expected one of: state, activity, agent, id, project"
        )),
    }
}

/// Parse one `session new --meta key=value` argument.
///
/// Used as clap's value parser, so a malformed pair fails the parse before any
/// connection is dialed (same usage-error surface as [`parse_list_filter`]).
/// Splits on the **first** `=` only, so a value may itself contain `=` (e.g. a
/// URL query string). Only shape is checked here — a key repeated across
/// separate `--meta` flags is caught later, once the full set is known, by
/// [`parse_meta_pairs`].
pub(crate) fn parse_meta_pair(input: &str) -> Result<(String, String), String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| format!("invalid --meta {input:?}: expected key=value"))?;
    if key.is_empty() {
        return Err(format!(
            "invalid --meta {input:?}: metadata key cannot be empty"
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// Build `SessionNewParams::metadata` from parsed `--meta` pairs.
///
/// Rejects a key named more than once: metadata travels as a flat map, so a
/// repeated key would otherwise silently collapse to whichever occurrence is
/// inserted last, which is a confusing outcome for a caller-visible CLI flag.
pub(crate) fn parse_meta_pairs(
    pairs: &[(String, String)],
) -> Result<BTreeMap<String, String>, CliError> {
    let mut metadata = BTreeMap::new();
    for (key, value) in pairs {
        if metadata.insert(key.clone(), value.clone()).is_some() {
            return Err(CliError::DuplicateMetaKey { key: key.clone() });
        }
    }
    Ok(metadata)
}

pub(crate) fn parse_state_filter(value: &str) -> Result<SessionState, String> {
    match value {
        "starting" => Ok(SessionState::Starting),
        "running" => Ok(SessionState::Running),
        "stopped" => Ok(SessionState::Stopped),
        "done" => Ok(SessionState::Done),
        "failed" => Ok(SessionState::Failed),
        other => Err(format!(
            "invalid state filter value {other:?}; expected one of: starting, running, stopped, done, failed"
        )),
    }
}

pub(crate) fn parse_activity_filter(value: &str) -> Result<AgentActivity, String> {
    match value {
        "working" => Ok(AgentActivity::Working),
        "blocked" => Ok(AgentActivity::Blocked),
        "idle" => Ok(AgentActivity::Idle),
        other => Err(format!(
            "invalid activity filter value {other:?}; expected one of: working, blocked, idle"
        )),
    }
}

fn parse_agent_filter(value: &str) -> Result<String, String> {
    // Free-form agent name (a base kind or a host profile); the daemon applies the
    // A.2.1 charset guard and resolves it. The client only rejects an empty value.
    if value.is_empty() {
        return Err("invalid agent filter value: name cannot be empty".to_owned());
    }
    Ok(value.to_owned())
}

fn session_matches_filters(session: &SessionInfo, filters: &[ListFilter]) -> bool {
    filters.iter().all(|filter| filter.matches(session))
}

/// Decide whether starting a session on `host` needs confirmation.
///
/// Local sessions always [`Proceed`](ConfirmDecision::Proceed). A remote session
/// requires either `--yes` (proceed), or — when no `--yes` — a machine path
/// (`json`) fails fast with [`RequireYes`](ConfirmDecision::RequireYes) while a
/// human path [`Prompt`](ConfirmDecision::Prompt)s.
#[must_use]
pub(crate) fn confirmation_decision(host: &str, json: bool, yes: bool) -> ConfirmDecision {
    let remote = !is_local_host(host);
    if !remote || yes {
        ConfirmDecision::Proceed
    } else if json {
        ConfirmDecision::RequireYes
    } else {
        ConfirmDecision::Prompt
    }
}

/// Refuse a remote `session new --input-stdin` that lacks `--yes`.
///
/// Stdin carries the input, so the confirmation prompt would read EOF and report
/// a decline. The call takes the machine path's rule instead, before stdin is
/// read and before any connection is opened.
///
/// # Errors
///
/// Returns [`CliError::RemoteConfirmationRequired`] for a remote `host` without
/// `yes`.
pub(crate) fn require_confirmation_for_stdin_input(host: &str, yes: bool) -> Result<(), CliError> {
    match confirmation_decision(host, true, yes) {
        ConfirmDecision::RequireYes => Err(CliError::RemoteConfirmationRequired),
        ConfirmDecision::Proceed | ConfirmDecision::Prompt => Ok(()),
    }
}

/// Run `session new` on `host`.
///
/// Remote starts pass through the confirmation gate ([`confirmation_decision`])
/// *before* any connection is opened; local starts are unaffected.
///
/// # Errors
///
/// Returns [`CliError`] if the remote start is unconfirmed, the daemon is
/// unreachable, the host cannot be resolved, the daemon rejects the request, or
/// the payload does not match the session contract.
pub(crate) async fn run_new(
    host: &str,
    paths: &Paths,
    args: NewArgs,
    json: bool,
    yes: bool,
) -> Result<(), CliError> {
    // Resolve the target shape before dialing: defaults the local cwd, and rejects
    // a remote start that names no project/repo (fail fast, no connection).
    let cwd_requested = args.cwd.is_some();
    let args = prepare_new_args(host, args)?;

    match confirmation_decision(host, json, yes) {
        ConfirmDecision::Proceed => {}
        ConfirmDecision::RequireYes => return Err(CliError::RemoteConfirmationRequired),
        ConfirmDecision::Prompt => {
            if !prompt_confirm(host)? {
                return Err(CliError::RemoteConfirmationDeclined {
                    host: host.to_owned(),
                });
            }
        }
    }

    let mut client = match args.request_timeout_ms {
        Some(timeout_ms) => {
            Client::connect_with_request_timeout(
                host,
                paths,
                Duration::from_millis(u64::from(timeout_ms)),
            )
            .await?
        }
        None => Client::connect(host, paths).await?,
    };
    // A daemon that predates the behavior would start the session in the
    // checkout root or the project checkout, so the request is refused before it
    // is sent.
    if cwd_requested || args.from_remote || args.input_ack_timeout_ms.is_some() {
        let capabilities = client.call::<method::HostInspect>(()).await?;
        if cwd_requested && !capabilities.session_new_cwd_supported {
            return Err(CliError::SessionNewCwdUnsupported {
                host: host.to_owned(),
                daemon_version: capabilities.daemon_version,
            });
        }
        if args.from_remote && !capabilities.session_new_branch_source_supported {
            return Err(CliError::SessionNewBranchSourceUnsupported {
                host: host.to_owned(),
                daemon_version: capabilities.daemon_version,
            });
        }
        if args.input_ack_timeout_ms.is_some() && !capabilities.session_new_input_ack_supported {
            return Err(CliError::SessionNewInputAckUnsupported {
                host: host.to_owned(),
                daemon_version: capabilities.daemon_version,
            });
        }
    }
    let result = client
        .call::<method::SessionNew>(new_params(&args)?)
        .await
        .map_err(|error| match error {
            CliError::Client(source @ pohunek_client::ClientError::RequestTimeout { .. }) => {
                CliError::SessionNewOutcomeUnknown(source)
            }
            other => other,
        })?;
    let info = &result.session;

    // We asked the daemon to inject an initial prompt but it did not confirm
    // doing so: it likely predates `session.new --input` support and silently
    // ignored the field. Warn on stderr (which never corrupts a `--json`
    // consumer's stdout) so a launcher does not falsely report a delivered
    // prompt; the session is still running, just without the preset input.
    if args.input.is_some() && result.applied_input != Some(true) {
        eprintln!(
            "pohunek: warning: host '{host}' did not confirm the initial --input was delivered; \
             it may be an older daemon that ignored it. The session is running without the preset \
             prompt."
        );
    }

    // The session exists either way, so a daemon that advertises the capability
    // but omits the outcome leaves the delivery unknown: it is not printed.
    let prompt_delivered = if args.input_ack_timeout_ms.is_some() {
        if result.prompt_delivered.is_none() {
            eprintln!(
                "pohunek: warning: host '{host}' did not report whether the prompt was delivered \
                 although it advertises `session_new_input_ack_supported`; session {} is running.",
                result.session.id.0
            );
        }
        result.prompt_delivered
    } else {
        None
    };
    if json {
        let mut value = serde_json::to_value(info)?;
        if let (Some(source), Some(object)) = (&result.worktree_source, value.as_object_mut()) {
            object.insert("worktree_source".to_owned(), serde_json::to_value(source)?);
        }
        if let (Some(delivered), Some(object)) = (prompt_delivered, value.as_object_mut()) {
            object.insert(
                "prompt_delivered".to_owned(),
                serde_json::Value::Bool(delivered),
            );
        }
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_NEW, &value)?
        );
    } else {
        print!(
            "{}",
            render_new_human(info, result.worktree_source.as_ref(), prompt_delivered)
        );
    }
    Ok(())
}

/// Prompt on stderr and read a yes/no answer from stdin.
///
/// Returns `true` only when the answer is `y`/`yes` (case-insensitive). The
/// prompt and echo go to stderr so a `--json` consumer is never affected (this
/// path is only reached when `json` is false). Dependency-free.
pub(crate) fn prompt_confirm(host: &str) -> Result<bool, CliError> {
    let mut stderr = std::io::stderr();
    write!(
        stderr,
        "Start a new session on remote host '{host}'? [y/N] "
    )?;
    stderr.flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

/// Run `session list` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// session contract.
pub(crate) async fn run_list(
    host: &str,
    paths: &Paths,
    filters: &[ListFilter],
    output_mode: ListOutputMode,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let sessions = client
        .call::<method::SessionList>(list_params(filters))
        .await?;

    print!("{}", render_list_output(&sessions, filters, output_mode)?);

    Ok(())
}

/// Run `session inspect` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_inspect(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect_pinned(host, paths).await?;
    let info = client
        .call::<method::SessionInspect>(SessionId(target.session_id.clone()))
        .await?;

    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_INSPECT, &info)?
        );
    } else {
        print!("{}", render_inspect_human(&info));
    }

    Ok(())
}

/// Run `session stop` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_stop(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let mut sdk = Client::connect_pinned(host, paths).await?.into_sdk();
    let stop = with_wait_notice(
        sdk.session_stop(SessionId(target.session_id.clone())),
        json,
        || {
            format!(
                "waiting for the daemon to stop {}: it waits for the session's processes to end",
                target.session_id
            )
        },
    )
    .await?;

    if json {
        print!("{}", crate::commands::render_json(&stop)?);
    } else {
        print!("{}", render_stop_human(&target.session_id, &stop));
    }
    Ok(())
}

/// Fork a session's native agent conversation into a new session.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the fork, or the payload does not match the
/// contract.
pub(crate) async fn run_fork(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    name: Option<String>,
    accept_profile_change: bool,
    json: bool,
) -> Result<(), CliError> {
    let mut sdk = Client::connect_pinned(host, paths).await?.into_sdk();
    let forked = with_wait_notice(
        sdk.session_fork(fork_params(
            target,
            name,
            accept_profile_change,
            DEFAULT_FORK_COLS,
            DEFAULT_FORK_ROWS,
        )),
        json,
        || {
            format!(
                "waiting for the daemon to fork {}: it starts the new session's worker first",
                target.session_id
            )
        },
    )
    .await?;
    let info = &forked.session;

    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_FORK, info)?
        );
    } else {
        print!("{}", render_fork_human(info));
    }
    Ok(())
}

/// Run `session rm` against the daemon for `host`.
///
/// Removal evicts the session from the daemon's registry, stopping it first if
/// it is still live. Unlike `stop`, the session no longer appears in `list`.
///
/// `accept_unconfirmed_cleanup` is the operator's consent, for this call only,
/// to remove a session whose runtime processes could not be proven gone solely
/// because same-user processes with unreadable environments may belong to it.
/// It selects the `session.remove_accepting_unconfirmed` method; without it
/// the refusing `session.remove` is called.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_remove(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    accept_unconfirmed_cleanup: bool,
    json: bool,
) -> Result<(), CliError> {
    let mut sdk = Client::connect_pinned(host, paths).await?.into_sdk();
    let session_id = SessionId(target.session_id.clone());
    let notice = || {
        format!(
            "waiting for the daemon to remove {}: it stops the session's processes first",
            target.session_id
        )
    };
    let removed = if accept_unconfirmed_cleanup {
        with_wait_notice(
            sdk.session_remove_accepting_unconfirmed(session_id),
            json,
            notice,
        )
        .await
        .map_err(missing_consent_method_hint)?
    } else {
        with_wait_notice(sdk.session_remove(session_id), json, notice).await?
    };

    if json {
        print!("{}", crate::commands::render_json(&removed)?);
    } else {
        print!("{}", render_remove_human(&target.session_id, &removed));
    }
    Ok(())
}

/// Run `session input` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_input(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    text: &str,
    wait_until: &[AgentActivity],
    timeout_ms: Option<u32>,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect_pinned(host, paths).await?;
    let params = input_params(target, text, wait_until, timeout_ms);
    let input = if params.wait.is_some() {
        let mut cancellation = ProcessCancellation::install()?;
        tokio::select! {
            result = client.session_input(params) => result?,
            () = cancellation.cancelled() => return Err(CliError::InputWaitInterrupted),
        }
    } else {
        client.session_input(params).await?
    };

    if json {
        print!("{}", crate::commands::render_json(&input)?);
    } else {
        print!("{}", render_input_human(&target.session_id, &input));
    }
    Ok(())
}

pub(crate) async fn run_screen(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let client = Client::connect_pinned(host, paths).await?;
    let result = client
        .into_sdk()
        .session_screen(SessionScreenParams::new(SessionId(
            target.session_id.clone(),
        )))
        .await?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        for line in result.visible_lines {
            println!("{line}");
        }
    }
    Ok(())
}

pub(crate) async fn run_detection(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let client = Client::connect_pinned(host, paths).await?;
    let result = client
        .into_sdk()
        .session_detection(SessionDetectionParams::new(SessionId(
            target.session_id.clone(),
        )))
        .await?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_detection_human(&result));
    }
    Ok(())
}

/// Arguments for a bounded `session read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadArgs {
    pub source: Option<SessionReadSource>,
    pub lines: Option<u32>,
    pub format: Option<SessionReadFormat>,
}

pub(crate) fn parse_read_source(value: &str) -> Result<SessionReadSource, String> {
    match value {
        "visible" => Ok(SessionReadSource::Visible),
        "recent" => Ok(SessionReadSource::Recent),
        "recent_unwrapped" | "recent-unwrapped" => Ok(SessionReadSource::RecentUnwrapped),
        "detection" => Ok(SessionReadSource::Detection),
        _ => Err(
            "source must be visible, recent, recent_unwrapped (alias: recent-unwrapped), or detection"
                .to_owned(),
        ),
    }
}

pub(crate) fn parse_read_format(value: &str) -> Result<SessionReadFormat, String> {
    match value {
        "text" => Ok(SessionReadFormat::Text),
        "ansi" => Ok(SessionReadFormat::Ansi),
        _ => Err("format must be text or ansi".to_owned()),
    }
}

pub(crate) fn parse_read_lines(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| "--lines must be an unsigned 32-bit integer".to_owned())?;
    if !(1..=MAX_SESSION_READ_LINES).contains(&parsed) {
        return Err(format!(
            "--lines must be between 1 and {MAX_SESSION_READ_LINES}"
        ));
    }
    Ok(parsed)
}

pub(crate) async fn run_read(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    args: ReadArgs,
    json: bool,
) -> Result<(), CliError> {
    let params = SessionReadParams::new(
        SessionId(target.session_id.clone()),
        args.source,
        args.lines,
        args.format,
    )
    .map_err(|error| CliError::InvalidObservation {
        detail: error.to_string(),
    })?;
    let mut client = Client::connect_pinned(host, paths).await?;
    let result = client.call::<method::SessionRead>(params).await?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        if result.truncated {
            eprintln!("warning: session read was truncated to the requested line limit");
        }
        print!("{}", render_read_text(&result));
    }
    Ok(())
}

fn render_detection_human(result: &SessionDetectionResult) -> String {
    let mut rendered = String::new();
    for preview in &result.previews {
        rendered.push_str("--- ");
        rendered.push_str(&preview.region);
        rendered.push_str(" ---\n");
        if preview.text.is_empty() {
            rendered.push_str("<empty>\n");
        } else {
            rendered.push_str(&preview.text);
            rendered.push('\n');
        }
    }
    rendered
}

fn render_read_text(result: &SessionReadResult) -> String {
    let mut output = result.text.clone();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Cursor and filtering arguments for `session output`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputArgs {
    pub worker_instance_id: Option<String>,
    pub runtime_generation: Option<u64>,
    pub after_offset: Option<u64>,
    pub max_bytes: u32,
    pub wait_ms: Option<u32>,
}

/// Predicate and cursor arguments for `session wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitArgs {
    pub worker_instance_id: Option<String>,
    pub runtime_generation: Option<u64>,
    pub after_updated_at: Option<String>,
    pub after_terminal_watermark: Option<u64>,
    pub after_output_offset: Option<u64>,
    pub states: Vec<SessionState>,
    pub activities: Vec<AgentActivity>,
    pub timeout_ms: u32,
}

fn runtime_identity(
    worker_instance_id: Option<String>,
    runtime_generation: Option<u64>,
) -> Result<Option<SessionRuntimeIdentity>, CliError> {
    match (worker_instance_id, runtime_generation) {
        (None, None) => Ok(None),
        (Some(worker_instance_id), Some(runtime_generation)) => SessionRuntimeIdentity::new(
            worker_instance_id,
            RuntimeGeneration::new(runtime_generation),
        )
        .map(Some)
        .map_err(|error| CliError::InvalidObservation {
            detail: error.to_string(),
        }),
        _ => Err(CliError::InvalidObservation {
            detail: "--worker-instance-id and --runtime-generation must be supplied together"
                .to_owned(),
        }),
    }
}

pub(crate) async fn run_output(
    host: &str,
    paths: &Paths,
    target: &Target,
    args: OutputArgs,
    json: bool,
) -> Result<(), CliError> {
    if usize::try_from(args.max_bytes).unwrap_or(usize::MAX) > MAX_SESSION_OUTPUT_BYTES {
        return Err(CliError::InvalidObservation {
            detail: format!(
                "--max-bytes must not exceed the protocol maximum of {MAX_SESSION_OUTPUT_BYTES}"
            ),
        });
    }
    let runtime = runtime_identity(args.worker_instance_id, args.runtime_generation)?;
    let after_offset = args.after_offset.map(OutputOffset::new);
    SessionOutputParams::new(
        SessionId(target.session_id.clone()),
        runtime.clone(),
        after_offset,
        args.max_bytes,
        args.wait_ms,
    )
    .map_err(|error| CliError::InvalidObservation {
        detail: error.to_string(),
    })?;
    // The session id resolved on one peer is only ever sent to that peer.
    let host = &PinnedHost::resolve(host).await?;
    let target = resolve_target_pinned(host, paths, target).await?;
    let params = SessionOutputParams::new(
        SessionId(target.session_id.clone()),
        runtime,
        after_offset,
        args.max_bytes,
        args.wait_ms,
    )
    .expect("only the already-validated session id changed");
    let client = Client::connect_pinned(host, paths).await?;
    let result = if args.wait_ms.is_some() {
        cancellable(client.into_sdk().session_output(params)).await?
    } else {
        client.into_sdk().session_output(params).await?
    };
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(result.data_base64())
            .map_err(|_error| CliError::InvalidObservation {
                detail: "daemon returned invalid output encoding".to_owned(),
            })?;
        print!("{}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

pub(crate) async fn run_wait(
    host: &str,
    paths: &Paths,
    target: &Target,
    args: WaitArgs,
    json: bool,
) -> Result<(), CliError> {
    let runtime = runtime_identity(args.worker_instance_id, args.runtime_generation)?;
    let states = (!args.states.is_empty()).then_some(args.states);
    let activities = (!args.activities.is_empty()).then_some(args.activities);
    let terminal_watermark = args.after_terminal_watermark.map(TerminalWatermark::new);
    let output_offset = args.after_output_offset.map(OutputOffset::new);
    SessionWaitParams::new(
        SessionId(target.session_id.clone()),
        runtime.clone(),
        args.after_updated_at.clone(),
        terminal_watermark,
        output_offset,
        states.clone(),
        activities.clone(),
        args.timeout_ms,
    )
    .map_err(|error| CliError::InvalidObservation {
        detail: error.to_string(),
    })?;
    let host = &PinnedHost::resolve(host).await?;
    let target = resolve_target_pinned(host, paths, target).await?;
    let params = SessionWaitParams::new(
        SessionId(target.session_id.clone()),
        runtime,
        args.after_updated_at,
        terminal_watermark,
        output_offset,
        states,
        activities,
        args.timeout_ms,
    )
    .expect("only the already-validated session id changed");
    let client = Client::connect_pinned(host, paths).await?;
    let result = cancellable(client.into_sdk().session_wait(params)).await?;
    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_WAIT, &result)?
        );
    } else {
        println!(
            "{:?}\t{}\t{}\t{}",
            result.reason,
            result.session.id.0,
            result.session.state.as_str(),
            result
                .session
                .activity
                .map_or("unknown", AgentActivity::as_str)
        );
    }
    Ok(())
}

/// Event, cursor, and duration arguments for a turn-mode `session wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnWaitArgs {
    /// Kinds that end the wait; empty selects every kind.
    pub until: Vec<TurnKindArg>,
    /// Cursor of an earlier result.
    pub after: Option<SessionTurnCursor>,
    /// Wait duration in milliseconds; `None` leaves it to the daemon default.
    pub timeout_ms: Option<u32>,
}

/// Wait for the next turn event of `targets`, which share one host.
///
/// Display names resolve to session ids with one `session.list` request; the
/// wait itself runs on a dedicated connection and ends early on a process
/// signal. Every completion reason, a timeout included, is a success.
pub(crate) async fn run_wait_turn(
    host: &str,
    paths: &Paths,
    targets: &[Target],
    args: TurnWaitArgs,
    json: bool,
) -> Result<(), CliError> {
    let until: Vec<SessionTurnEventKind> = if args.until.is_empty() {
        ALL_TURN_KINDS.to_vec()
    } else {
        args.until.into_iter().map(Into::into).collect()
    };
    let params = |session_ids: Vec<SessionId>| {
        SessionWaitTurnParams::new(
            session_ids,
            until.clone(),
            args.after.clone(),
            args.timeout_ms,
        )
        .map_err(|error| CliError::InvalidObservation {
            detail: error.to_string(),
        })
    };
    params(
        targets
            .iter()
            .map(|target| SessionId(target.session_id.clone()))
            .collect(),
    )?;
    let mut sdk = Client::connect(host, paths).await?.into_sdk();
    let sessions = sdk
        .call::<protocol::method::SessionList>(SessionListParams {
            filters: Vec::new(),
        })
        .await?;
    let session_ids = targets
        .iter()
        .map(|target| {
            resolve_target_from_sessions(target, &sessions)
                .map(|target| SessionId(target.session_id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Two targets can name one session, for example its id and its display name.
    let params = params(session_ids)?;
    let result = cancellable(sdk.session_wait_turn(params)).await?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_turn_wait(&result));
    }
    Ok(())
}

/// Placeholder for a progress field the daemon has not observed.
const UNOBSERVED: &str = "-";

/// Render a turn-wait result as human text.
///
/// The first line names the event (`<session> <kind> <at>`) or the reason the
/// wait ended without one; one `progress:` line per target follows, then the
/// `cursor:` line a shell loop passes to the next `--after`.
fn render_turn_wait(result: &SessionWaitTurnResult) -> String {
    let mut output = String::new();
    match &result.event {
        Some(event) => {
            let _ = writeln!(output, "{} {} {}", event.session_id.0, event.kind, event.at);
        }
        None => {
            let _ = writeln!(output, "{}", turn_wait_reason(result.reason));
        }
    }
    for progress in &result.progress {
        let _ = writeln!(output, "progress: {}", render_turn_progress(progress));
    }
    let _ = writeln!(output, "cursor: {}", result.cursor);
    output
}

fn turn_wait_reason(reason: SessionWaitTurnReason) -> &'static str {
    match reason {
        SessionWaitTurnReason::Event => "event",
        SessionWaitTurnReason::Timeout => "timeout",
        SessionWaitTurnReason::CursorReset => "cursor_reset",
        SessionWaitTurnReason::TargetsEnded => "targets_ended",
    }
}

fn render_turn_progress(progress: &SessionTurnProgress) -> String {
    format!(
        "{} {} {} output_advanced={} last_output_at={} turn_started_at={}",
        progress.session_id.0,
        progress.state.as_str(),
        progress.activity.map_or("unknown", AgentActivity::as_str),
        progress.output_advanced,
        progress.last_output_at.as_deref().unwrap_or(UNOBSERVED),
        progress.turn_started_at.as_deref().unwrap_or(UNOBSERVED),
    )
}

pub(crate) async fn run_resume(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    accept_profile_change: bool,
    json: bool,
) -> Result<(), CliError> {
    let mut sdk = Client::connect_pinned(host, paths).await?.into_sdk();
    let result = with_wait_notice(
        sdk.session_resume_with(resume_params(target, accept_profile_change)),
        json,
        || {
            format!(
                "waiting for the daemon to resume {}: it first proves the previous runtime ended",
                target.session_id
            )
        },
    )
    .await?;
    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_RESUME, &result)?
        );
    } else {
        println!("resumed {}", result.session.id.0);
    }
    Ok(())
}

/// Awaits a lifecycle `call`. Once it has waited
/// [`LIFECYCLE_PROGRESS_NOTICE_AFTER`], prints `notice` to stderr (never to
/// stdout, which `--json` consumers parse) unless `json` is set.
async fn with_wait_notice<T>(
    call: impl std::future::Future<Output = Result<T, pohunek_client::ClientError>>,
    json: bool,
    notice: impl FnOnce() -> String,
) -> Result<T, CliError> {
    tokio::pin!(call);
    let result = tokio::select! {
        result = &mut call => result,
        () = tokio::time::sleep(LIFECYCLE_PROGRESS_NOTICE_AFTER) => {
            if !json {
                eprintln!("{}", notice());
            }
            call.await
        }
    };
    result.map_err(crate::client::map_client_error)
}

pub(crate) async fn run_resize(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    cols: u16,
    rows: u16,
    json: bool,
) -> Result<(), CliError> {
    let client = Client::connect_pinned(host, paths).await?;
    let result = client
        .into_sdk()
        .session_resize(SessionResizeParams {
            session_id: SessionId(target.session_id.clone()),
            cols,
            rows,
        })
        .await?;
    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_RESIZE, &result)?
        );
    } else {
        println!("resized {} to {cols}x{rows}", target.session_id);
    }
    Ok(())
}

pub(crate) async fn run_metadata(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    metadata: BTreeMap<String, Option<String>>,
    json: bool,
) -> Result<(), CliError> {
    let client = Client::connect_pinned(host, paths).await?;
    let result = client
        .into_sdk()
        .session_set_metadata(SessionSetMetadataParams {
            session_id: SessionId(target.session_id.clone()),
            metadata,
        })
        .await?;
    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_SET_METADATA, &result)?
        );
    } else {
        println!("updated metadata for {}", target.session_id);
    }
    Ok(())
}

pub(crate) async fn run_runtime_inventory(
    host: &str,
    paths: &Paths,
    json: bool,
) -> Result<(), CliError> {
    let client = Client::connect(host, paths).await?;
    let result = client.into_sdk().session_runtime_inventory().await?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        for entry in result.entries {
            println!("{}\t{:?}", entry.runtime_slot, entry.status);
        }
    }
    Ok(())
}

/// Set (`Some`) or clear (`None`) a session's display name on the target host.
pub(crate) async fn run_rename(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    name: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect_pinned(host, paths).await?;
    let renamed = client
        .call::<method::SessionRename>(rename_params(target, name))
        .await?;

    if json {
        print!(
            "{}",
            crate::commands::render_json_with_v3_identity(method::SESSION_RENAME, &renamed)?
        );
    } else {
        print!("{}", render_rename_human(&renamed.session));
    }
    Ok(())
}

/// Run `session diff` against the daemon for `host`.
///
/// The default (non-`--json`) output is the raw unified diff text on stdout,
/// not a table: this is the reference-client parity path other tools pipe
/// through, so stdout must stay exactly the diff. A truncation warning is
/// printed to stderr instead, so it never corrupts a piped diff.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request (e.g. the session has no bound
/// worktree), or the payload does not match the contract.
pub(crate) async fn run_diff(
    host: &PinnedHost,
    paths: &Paths,
    target: &Target,
    base: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect_pinned(host, paths).await?;
    let result = client
        .call::<method::SessionDiff>(diff_params(target, base))
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        if result.truncated {
            eprintln!(
                "warning: diff for session {} was truncated to fit the size cap",
                target.session_id
            );
        }
        print!("{}", result.diff);
    }
    Ok(())
}

/// Fields accepted by `session policy set`.
///
/// Every field is optional so an update is a read-modify-write of exactly the
/// fields the operator named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PolicyArgs {
    /// Enable automatic retention sweeps.
    pub(crate) enabled: bool,
    /// Disable automatic retention sweeps.
    pub(crate) disabled: bool,
    /// Seconds between automatic sweeps.
    pub(crate) sweep_interval_secs: Option<u32>,
    /// Seconds a terminal session is kept.
    pub(crate) terminal_ttl_secs: Option<u32>,
    /// Seconds a session whose runtime is lost is kept.
    pub(crate) lost_ttl_secs: Option<u32>,
    /// Maximum sessions one sweep may remove.
    pub(crate) max_removals_per_sweep: Option<u32>,
    /// Idle-stop default in seconds; 0 clears it.
    pub(crate) idle_stop_after: Option<u32>,
}

impl PolicyArgs {
    /// Whether any field of the retention policy is named.
    fn names_retention(self) -> bool {
        self.enabled
            || self.disabled
            || self.sweep_interval_secs.is_some()
            || self.terminal_ttl_secs.is_some()
            || self.lost_ttl_secs.is_some()
            || self.max_removals_per_sweep.is_some()
    }
}

/// What `session policy get` and `set` print: the retention policy as the
/// daemon holds it and, from a daemon that serves it, the idle-stop default.
#[derive(Debug, serde::Serialize)]
struct PolicyOutput {
    retention: SessionRetentionPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_stop: Option<SessionIdleStopResult>,
}

/// Whether a daemon answered that it does not serve the idle-stop methods: a
/// daemon of the previous release answers `method_not_found`, and a protocol 3
/// daemon is refused before anything is sent.
fn idle_stop_unsupported(error: &CliError) -> bool {
    match error {
        CliError::Protocol(source) => source.code == "method_not_found",
        CliError::Client(pohunek_client::ClientError::DaemonProtocolTooOld { .. }) => true,
        CliError::Client(pohunek_client::ClientError::RemoteProtocol { source, .. }) => {
            source.code == "method_not_found"
        }
        _ => false,
    }
}

/// Recovery hint for a daemon that does not know the idle-stop default;
/// `connected` is the version the daemon reported, when it answered `daemon.health`.
///
/// The upgrade target is this CLI's own release, which serves the methods it
/// calls, so the hint names no release number of its own.
fn idle_stop_upgrade_hint(connected: Option<&str>) -> String {
    let daemon = connected.map_or_else(
        || "the connected daemon".to_owned(),
        |version| format!("the connected daemon (version {version})"),
    );
    format!(
        "{daemon} predates --idle-stop-after; upgrade the daemon to this CLI's release ({cli}) and retry. Nothing was changed",
        cli = env!("CARGO_PKG_VERSION"),
    )
}

/// Adds an upgrade hint naming the connected daemon's version to a
/// `method_not_found` answer for the idle-stop default. Errors relayed from a
/// remote daemon keep their host.
async fn missing_idle_stop_method_hint(client: &mut Client, error: CliError) -> CliError {
    let is_missing = |source: &protocol::ProtocolError| source.code == "method_not_found";
    let missing = match &error {
        CliError::Protocol(source) => is_missing(source),
        CliError::Client(pohunek_client::ClientError::RemoteProtocol { source, .. }) => {
            is_missing(source)
        }
        _ => false,
    };
    if !missing {
        return error;
    }
    let connected = client
        .call::<method::DaemonHealth>(())
        .await
        .ok()
        .map(|health| health.daemon_version);
    let hint = idle_stop_upgrade_hint(connected.as_deref());
    let add_hint = |mut source: protocol::ProtocolError| {
        source.recover = Some(hint.clone());
        source
    };
    match error {
        CliError::Protocol(source) => CliError::Protocol(add_hint(source)),
        CliError::Client(pohunek_client::ClientError::RemoteProtocol { host, source }) => {
            CliError::Client(pohunek_client::ClientError::RemoteProtocol {
                host,
                source: add_hint(source),
            })
        }
        other => other,
    }
}

/// Reads the idle-stop default, or `None` from a daemon that does not serve it.
async fn read_idle_stop(client: &mut Client) -> Result<Option<SessionIdleStopResult>, CliError> {
    match client.call::<method::SessionIdleStopGet>(()).await {
        Ok(result) => Ok(Some(result)),
        Err(error) => {
            if idle_stop_unsupported(&error) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

/// Arguments accepted by `session retention sweep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepArgs {
    /// Report the selection without removing anything.
    pub(crate) dry_run: bool,
    /// Remove the selected sessions.
    pub(crate) apply: bool,
    /// Maximum sessions to remove.
    pub(crate) limit: Option<u32>,
}

/// Run `session policy get` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_policy_get(host: &str, paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let result = client.call::<method::SessionPolicyGet>(()).await?;
    let idle_stop = read_idle_stop(&mut client).await?;
    print_policy(host, result.retention, idle_stop, true, json)
}

fn print_policy(
    host: &str,
    retention: SessionRetentionPolicy,
    idle_stop: Option<SessionIdleStopResult>,
    report_idle_stop: bool,
    json: bool,
) -> Result<(), CliError> {
    if json {
        print!(
            "{}",
            crate::commands::render_json(&PolicyOutput {
                retention,
                idle_stop
            })?
        );
    } else {
        print!(
            "{}",
            render_policy_human(host, &retention, idle_stop.as_ref(), report_idle_stop)
        );
    }
    Ok(())
}

/// Run `session policy set` against the daemon for `host`.
///
/// The idle-stop default is only touched when `--idle-stop-after` is named. The
/// daemon's support and minimum are read and the value is checked first, then
/// the retention fields are written, then the default, so a refusal of either
/// changes nothing. A daemon that refuses the default after the retention
/// policy was written says which half landed. The current retention policy is
/// read first so unnamed fields keep their value, which also means the daemon
/// validates exactly the policy it will store.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the policy, or the payload does not match the
/// contract.
pub(crate) async fn run_policy_set(
    host: &str,
    paths: &Paths,
    args: PolicyArgs,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    if let Some(secs) = args.idle_stop_after {
        let served = match client.call::<method::SessionIdleStopGet>(()).await {
            Ok(served) => served,
            Err(error) => return Err(missing_idle_stop_method_hint(&mut client, error).await),
        };
        if secs != 0 && secs < served.min_after_secs {
            return Err(CliError::Protocol(protocol::ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "invalid_session_policy",
                format!(
                    "--idle-stop-after must be 0 or at least {} seconds",
                    served.min_after_secs
                ),
                None,
            )));
        }
    }
    let current = client.call::<method::SessionPolicyGet>(()).await?.retention;
    let retention_written = args.names_retention();
    let retention = if retention_written {
        client
            .call::<method::SessionPolicySet>(SessionPolicyParams {
                retention: apply_policy_args(current, args),
            })
            .await?
            .retention
    } else {
        current
    };
    let idle_stop = match args.idle_stop_after {
        Some(secs) => Some(
            match client
                .call::<method::SessionIdleStopSet>(SessionIdleStopParams {
                    default_after_secs: Some(secs),
                })
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    let error = missing_idle_stop_method_hint(&mut client, error).await;
                    return Err(if retention_written {
                        note_retention_landed(error)
                    } else {
                        error
                    });
                }
            },
        ),
        None => None,
    };
    print_policy(
        host,
        retention,
        idle_stop,
        args.idle_stop_after.is_some(),
        json,
    )
}

/// Says in `error` that the retention policy was written before the failure.
fn note_retention_landed(error: CliError) -> CliError {
    const LANDED: &str = "the retention policy was updated, but the idle-stop default was not";
    let note = |mut source: protocol::ProtocolError| {
        source.msg = format!("{LANDED}: {}", source.msg);
        source
    };
    match error {
        CliError::Protocol(source) => CliError::Protocol(note(source)),
        CliError::Client(pohunek_client::ClientError::RemoteProtocol { host, source }) => {
            CliError::Client(pohunek_client::ClientError::RemoteProtocol {
                host,
                source: note(source),
            })
        }
        other => {
            eprintln!("{LANDED}");
            other
        }
    }
}

/// Run `session retention sweep` against the daemon for `host`.
///
/// Returns whether the sweep completed without any partial failure, so the
/// caller can turn a partial failure into a non-zero exit status for cron jobs
/// and health checks that only inspect it.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_retention_sweep(
    host: &str,
    paths: &Paths,
    args: SweepArgs,
    json: bool,
) -> Result<bool, CliError> {
    let mut client = Client::connect(host, paths).await?;
    let result = client
        .call::<method::SessionRetentionSweep>(sweep_params(args))
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_sweep_human(host, &result));
    }
    Ok(sweep_succeeded(&result))
}

/// Returns whether a sweep did everything it set out to do.
///
/// A session it could not remove and a worktree it could not delete are both
/// partial failures the caller must be able to see without parsing the output. A
/// held session is not: keeping work is the sweep behaving correctly.
fn sweep_succeeded(result: &SessionRetentionResult) -> bool {
    result.failed == 0 && result.worktrees_failed == 0
}

/// Returns `current` with the fields named by `args` replaced.
fn apply_policy_args(current: SessionRetentionPolicy, args: PolicyArgs) -> SessionRetentionPolicy {
    let enabled = if args.enabled {
        true
    } else if args.disabled {
        false
    } else {
        current.enabled
    };
    SessionRetentionPolicy {
        enabled,
        sweep_interval_secs: args
            .sweep_interval_secs
            .unwrap_or(current.sweep_interval_secs),
        terminal_ttl_secs: args.terminal_ttl_secs.unwrap_or(current.terminal_ttl_secs),
        lost_ttl_secs: args.lost_ttl_secs.unwrap_or(current.lost_ttl_secs),
        max_removals_per_sweep: args
            .max_removals_per_sweep
            .unwrap_or(current.max_removals_per_sweep),
    }
}

/// Builds the sweep params. `--apply` is the absence of `--dry-run`; clap
/// requires exactly one of the two so the destructive mode is never implicit.
fn sweep_params(args: SweepArgs) -> SessionRetentionParams {
    SessionRetentionParams {
        dry_run: args.dry_run || !args.apply,
        limit: args.limit,
    }
}

fn render_policy_human(
    host: &str,
    policy: &SessionRetentionPolicy,
    idle_stop: Option<&SessionIdleStopResult>,
    report_idle_stop: bool,
) -> String {
    let mut output = format!("host {host} session retention policy\n");
    let _ = writeln!(
        output,
        "  automatic sweeps: {}",
        if policy.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    let _ = writeln!(
        output,
        "  sweep_interval_secs: {}",
        policy.sweep_interval_secs
    );
    let _ = writeln!(output, "  terminal_ttl_secs: {}", policy.terminal_ttl_secs);
    let _ = writeln!(output, "  lost_ttl_secs: {}", policy.lost_ttl_secs);
    let _ = writeln!(
        output,
        "  max_removals_per_sweep: {}",
        policy.max_removals_per_sweep
    );
    match idle_stop {
        None if !report_idle_stop => {}
        Some(idle) => {
            let _ = writeln!(
                output,
                "  idle_stop_after: {} (minimum {} seconds)",
                idle.default_after_secs
                    .map_or_else(|| "not set".to_owned(), |secs| format!("{secs} seconds")),
                idle.min_after_secs
            );
        }
        None => {
            let _ = writeln!(output, "  idle_stop_after: not supported by this daemon");
        }
    }
    output
}

fn render_sweep_human(host: &str, result: &SessionRetentionResult) -> String {
    let action = if result.dry_run { "matched" } else { "removed" };
    let count = if result.dry_run {
        result.eligible
    } else {
        result.removed
    };
    let mut output = format!(
        "host {host} session retention {action} {count} of {} session(s)\n",
        result.examined
    );
    let _ = writeln!(
        output,
        "  eligible={} held={} removed={} worktrees_cleaned={} worktrees_failed={} failed={}",
        result.eligible,
        result.held,
        result.removed,
        result.worktrees_cleaned,
        result.worktrees_failed,
        result.failed
    );
    for candidate in &result.sessions {
        let worktree = candidate
            .worktree_path
            .as_ref()
            .map_or_else(|| "-".to_owned(), |path| path.display().to_string());
        let hold = candidate
            .hold
            .map_or("-", protocol::SessionRetentionHold::as_str);
        let _ = writeln!(
            output,
            "  {} reason={} age_secs={} hold={hold} removed={} worktree={worktree}",
            candidate.session_id.0,
            candidate.reason.as_str(),
            candidate.age_secs,
            candidate.removed
        );
    }
    output
}

fn new_params(args: &NewArgs) -> Result<SessionNewParams, CliError> {
    Ok(SessionNewParams {
        agent: args.agent.clone(),
        name: args.name.clone(),
        cwd: args.cwd.clone(),
        cols: args.cols,
        rows: args.rows,
        project: args.project.clone(),
        repo: args.repo.clone(),
        branch: args.branch.clone(),
        base_branch: args.base_branch.clone(),
        input: args.input.clone(),
        extended_input_ready_wait: None,
        branch_source: args.from_remote.then_some(protocol::BranchSource::Remote),
        on_diverged: args
            .reset_diverged
            .then_some(protocol::OnDiverged::BackupReset),
        input_ack_timeout_ms: args.input_ack_timeout_ms,
        metadata: parse_meta_pairs(&args.meta)?,
    })
}

#[cfg(test)]
fn build_new_request(args: &NewArgs) -> Result<Request, CliError> {
    request_with_params(method::SESSION_NEW, &new_params(args)?)
}

/// Prepare the `session new` args for the target host (design Decision 1).
///
/// A requested `--cwd` is checked before any connection: it conflicts with
/// `--branch` (the session runs in its worktree), a remote one must be absolute
/// (the path is read on that host), and a local relative one is made absolute
/// against the CLI's own directory. Without `--cwd`, a local start that names
/// neither `--project` nor `--repo` sends the root of the git work tree around the
/// CLI's directory (the directory itself outside a work tree) so the daemon
/// auto-detects the project we are standing in. A remote start sends
/// no default path and requires a `--project` reference (or `--repo` for
/// first-introduction), failing fast before any connection is dialed.
fn prepare_new_args(host: &str, args: NewArgs) -> Result<NewArgs, CliError> {
    let remote = !is_local_host(host);
    if remote && args.project.is_none() && args.repo.is_none() {
        return Err(CliError::RemoteTargetRequired);
    }
    let cwd = match args.cwd {
        Some(_) if args.branch.is_some() => return Err(CliError::CwdWithWorktree),
        Some(cwd) if cwd.is_absolute() => Some(cwd),
        Some(cwd) if remote => return Err(CliError::RemoteCwdNotAbsolute { cwd }),
        Some(cwd) => Some(std::env::current_dir()?.join(cwd)),
        None if !remote && args.project.is_none() && args.repo.is_none() => {
            Some(implicit_launch_dir(&std::env::current_dir()?))
        }
        None => None,
    };
    Ok(NewArgs { cwd, ..args })
}

/// Longest the work-tree root lookup of [`implicit_launch_dir`] may take: an
/// unresponsive git must not stall a session start that does not need it.
const IMPLICIT_ROOT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll interval while waiting for the work-tree root lookup to finish.
const IMPLICIT_ROOT_LOOKUP_POLL: Duration = Duration::from_millis(10);

/// The directory a local start without `--cwd` runs in: the root of the git
/// work tree around `dir`, so standing in a subdirectory still starts the
/// session at the checkout root, or `dir` itself outside any work tree.
///
/// A lookup that fails for any other reason (git missing, too slow, refusing
/// the directory) warns on stderr and starts the session in `dir`.
fn implicit_launch_dir(dir: &Path) -> PathBuf {
    match work_tree_root(dir) {
        Ok(Some(root)) => root,
        Ok(None) => dir.to_path_buf(),
        Err(reason) => {
            eprintln!(
                "pohunek: warning: could not find the git work tree root of {} ({reason}); \
                 the session starts in that directory",
                dir.display()
            );
            dir.to_path_buf()
        }
    }
}

/// `git rev-parse --show-toplevel` in `dir`, canonicalized.
///
/// `Ok(None)` when `dir` is outside a work tree; `Err` with the reason when git
/// cannot be run, exceeds [`IMPLICIT_ROOT_LOOKUP_TIMEOUT`], or fails otherwise.
fn work_tree_root(dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut command = std::process::Command::new("git");
    // `GIT_*` variables (`GIT_DIR`, `GIT_WORK_TREE`, ...) redirect git away from
    // the directory it is asked about; only the ceiling, which limits where it
    // looks, is kept. The C locale makes its messages stable.
    for (name, _) in std::env::vars_os() {
        let name_text = name.to_string_lossy();
        if name_text.starts_with("GIT_") && name_text != GIT_CEILING_VARIABLE {
            command.env_remove(&name);
        }
    }
    let mut child = command
        .env("LC_ALL", "C")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run git: {error}"))?;
    let deadline = std::time::Instant::now() + IMPLICIT_ROOT_LOOKUP_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|error| format!("git: {error}"))? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git did not answer in time".to_owned());
            }
            None => std::thread::sleep(IMPLICIT_ROOT_LOOKUP_POLL),
        }
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output)
            .map_err(|error| format!("git output: {error}"))?;
    }
    let mut diagnostics = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut stderr, &mut diagnostics);
    }
    if !status.success() {
        if NOT_A_WORK_TREE_MESSAGES
            .iter()
            .any(|message| diagnostics.contains(message))
        {
            return Ok(None);
        }
        return Err(format!("git: {}", diagnostics.trim()));
    }
    let root = PathBuf::from(output.trim());
    if root.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(root.canonicalize().unwrap_or(root)))
}

/// Git variable that bounds the upward search for a repository.
const GIT_CEILING_VARIABLE: &str = "GIT_CEILING_DIRECTORIES";

/// What git prints (in the C locale) for a directory outside any work tree: no
/// repository at all, or a bare repository.
const NOT_A_WORK_TREE_MESSAGES: [&str; 2] = ["not a git repository", "must be run in a work tree"];

fn list_params(filters: &[ListFilter]) -> SessionListParams {
    SessionListParams {
        filters: filters.iter().map(ListFilter::to_protocol_filter).collect(),
    }
}

// Host routing is handled by the transport ([`Client`]); these requests carry
// only the session id (identical on either side), never the host.
#[cfg(test)]
fn build_list_request(filters: &[ListFilter]) -> Result<Request, CliError> {
    request_with_params(method::SESSION_LIST, &list_params(filters))
}

/// Recovery hint for a daemon that does not know the consent method.
const CONSENT_METHOD_UPGRADE_HINT: &str =
    "the daemon predates --accept-unconfirmed-cleanup; upgrade the daemon and retry";

/// Adds an upgrade hint to a `method_not_found` answer for the consent
/// method, which only daemons that support it accept. Errors relayed from a
/// remote daemon keep their host.
fn missing_consent_method_hint(error: CliError) -> CliError {
    let add_hint = |mut source: protocol::ProtocolError| {
        if source.code == "method_not_found" {
            source.recover = Some(CONSENT_METHOD_UPGRADE_HINT.to_owned());
        }
        source
    };
    match error {
        CliError::Protocol(source) => CliError::Protocol(add_hint(source)),
        CliError::Client(pohunek_client::ClientError::RemoteProtocol { host, source }) => {
            CliError::Client(pohunek_client::ClientError::RemoteProtocol {
                host,
                source: add_hint(source),
            })
        }
        other => other,
    }
}

fn fork_params(
    target: &Target,
    name: Option<String>,
    accept_profile_change: bool,
    cols: u16,
    rows: u16,
) -> SessionForkParams {
    SessionForkParams {
        session_id: SessionId(target.session_id.clone()),
        name,
        cwd_mode: ForkCwdMode::Same,
        cols,
        rows,
        accept_profile_change,
    }
}

fn resume_params(target: &Target, accept_profile_change: bool) -> SessionResumeParams {
    SessionResumeParams::new(SessionId(target.session_id.clone()), accept_profile_change)
}

#[cfg(test)]
fn build_fork_request(
    target: &Target,
    name: Option<String>,
    accept_profile_change: bool,
    cols: u16,
    rows: u16,
) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_FORK,
        &fork_params(target, name, accept_profile_change, cols, rows),
    )
}

fn input_params(
    target: &Target,
    text: &str,
    wait_until: &[AgentActivity],
    timeout_ms: Option<u32>,
) -> SessionInputParams {
    SessionInputParams {
        session_id: SessionId(target.session_id.clone()),
        text: text.to_owned(),
        wait: (!wait_until.is_empty() || timeout_ms.is_some()).then(|| SessionInputWait {
            until: Some(wait_until.to_vec()),
            timeout_ms,
        }),
    }
}

#[cfg(test)]
fn build_input_request(target: &Target, text: &str) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_INPUT,
        &input_params(target, text, &[], None),
    )
}

fn rename_params(target: &Target, name: Option<String>) -> SessionRenameParams {
    SessionRenameParams {
        session_id: SessionId(target.session_id.clone()),
        name,
    }
}

#[cfg(test)]
fn build_rename_request(target: &Target, name: Option<String>) -> Result<Request, CliError> {
    request_with_params(method::SESSION_RENAME, &rename_params(target, name))
}

fn diff_params(target: &Target, base: Option<String>) -> SessionDiffParams {
    SessionDiffParams {
        session_id: SessionId(target.session_id.clone()),
        base,
    }
}

#[cfg(test)]
fn build_diff_request(target: &Target, base: Option<String>) -> Result<Request, CliError> {
    request_with_params(method::SESSION_DIFF, &diff_params(target, base))
}

fn render_new_human(
    info: &SessionInfo,
    source: Option<&protocol::WorktreeSource>,
    prompt_delivered: Option<bool>,
) -> String {
    let mut output = format!(
        "session {} created (state: {})\n",
        info.id.0,
        state_label(info.state)
    );
    if let Some(path) = &info.worktree_path {
        let branch = info
            .branch
            .as_deref()
            .map(|b| format!(" (branch {b})"))
            .unwrap_or_default();
        let _ = writeln!(output, "  worktree: {}{branch}", path.display());
    }
    if let Some(source) = source {
        let _ = writeln!(
            output,
            "  worktree source: {} at {}",
            source.source_ref, source.commit
        );
        if let Some(previous) = &source.fast_forwarded_from {
            let _ = writeln!(output, "  fast-forwarded from: {previous}");
        }
        if let Some(backup) = &source.backup_ref {
            let _ = writeln!(output, "  previous local head kept as: {backup}");
        }
    }
    if let Some(delivered) = prompt_delivered {
        let _ = writeln!(
            output,
            "  prompt delivered: {}",
            if delivered { "yes" } else { "no" }
        );
    }
    write_warnings(&mut output, &info.warnings);
    output
}

fn render_fork_human(info: &SessionInfo) -> String {
    format!(
        "session {} forked (state: {})\n",
        info.id.0,
        state_label(info.state)
    )
}

fn render_list_human(sessions: &[SessionInfo]) -> String {
    let id_width = sessions
        .iter()
        .map(|s| s.id.0.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());
    let name_width = sessions
        .iter()
        .map(|s| name_label(s).len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let agent_width = sessions
        .iter()
        .map(|s| session_agent_label(s).len())
        .max()
        .unwrap_or(0)
        .max("AGENT".len());
    let project_width = sessions
        .iter()
        .map(|s| project_label(s).len())
        .max()
        .unwrap_or(0)
        .max("PROJECT".len());
    let branch_width = sessions
        .iter()
        .map(|s| branch_label(s).len())
        .max()
        .unwrap_or(0)
        .max("BRANCH".len());
    let background_width = sessions
        .iter()
        .map(|s| background_count_label(s).len())
        .max()
        .unwrap_or(0)
        .max("BACKGROUND".len());

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{:<id_width$}  {:<name_width$}  {:<agent_width$}  {:<8}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {:<project_width$}  {:<branch_width$}  {:<4}  {:<9}  {:<background_width$}  CWD",
        "ID", "NAME", "AGENT", "ORIGIN", "STATE", "ACTIVITY", "SOURCE", "PID", "SIZE", "PROJECT", "BRANCH", "WARN", "SUBAGENTS", "BACKGROUND",
    );
    for session in sessions {
        let _ = writeln!(
            output,
            "{:<id_width$}  {:<name_width$}  {:<agent_width$}  {:<8}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {:<project_width$}  {:<branch_width$}  {:<4}  {:<9}  {:<background_width$}  {}",
            session.id.0,
            name_label(session),
            session_agent_label(session),
            origin_label(session),
            state_label(session.state),
            activity_label_option(session.activity),
            state_source_label(session.state_source),
            session.pid,
            format!("{}x{}", session.cols, session.rows),
            project_label(session),
            branch_label(session),
            warn_count_label(session),
            subagent_count_label(session),
            background_count_label(session),
            session.cwd.display(),
        );
    }
    render_list_footer(&mut output, sessions);
    output
}

/// Appends the per-session lines that the table's columns only count: the
/// native recovery target, the refused capabilities and native recovery
/// warnings.
fn render_list_footer(output: &mut String, sessions: &[SessionInfo]) {
    // The WARN column only counts; a lost native recovery record needs its
    // recovery hint where the operator looks for the session.
    for session in sessions {
        if let Some(target) = session
            .native_session_id
            .as_deref()
            .or(session.native_session_path.as_deref())
        {
            let _ = writeln!(
                output,
                "recovery {}: target={} last_activity={}",
                session.id.0,
                target,
                session
                    .native_last_activity_at
                    .as_deref()
                    .unwrap_or("<unknown>")
            );
        }
        if let Some(refusals) = &session.capability_refusals {
            let _ = writeln!(
                output,
                "refused {}: {}",
                session.id.0,
                capability_refusals_label(refusals)
            );
        }
        for warning in session
            .warnings
            .iter()
            .filter(|warning| warning.kind == SessionWarningKind::NativeRecovery)
        {
            let _ = writeln!(
                output,
                "warning [{}] {}: {}",
                warning_kind_label(warning.kind),
                session.id.0,
                warning.message
            );
        }
    }
}

fn render_list_quiet(sessions: &[SessionInfo]) -> String {
    let mut output = String::new();
    for session in sessions {
        output.push_str(&session.id.0);
        output.push('\n');
    }
    output
}

fn render_list_output(
    sessions: &[SessionInfo],
    filters: &[ListFilter],
    output_mode: ListOutputMode,
) -> Result<String, CliError> {
    // Defense-in-depth re-filter: the daemon already applies these filters
    // (`build_list_request` sends them as typed params). Re-applying the
    // predicate also protects legacy filters from peers that ignore unknown
    // params. Metadata filters are gated by protocol negotiation before send,
    // so a protocol-3 daemon cannot silently ignore them.
    let filtered: Vec<SessionInfo> = sessions
        .iter()
        .filter(|session| session_matches_filters(session, filters))
        .cloned()
        .collect();

    match output_mode {
        ListOutputMode::Human => Ok(render_list_human(&filtered)),
        ListOutputMode::Json => {
            crate::commands::render_json_with_v3_identity(method::SESSION_LIST, &filtered)
        }
        ListOutputMode::Quiet => Ok(render_list_quiet(&filtered)),
    }
}

/// Project column value: the project's display label, falling back to its derived
/// id when the label is unresolved (e.g. the project was removed), or `-` for a
/// session with no git project.
fn project_label(info: &SessionInfo) -> String {
    info.project_label
        .clone()
        .or_else(|| info.project_id.clone())
        .unwrap_or_else(|| "-".to_owned())
}

/// Name column value: the owner-set display name, or `-` when the session has
/// none (it is then shown by its id).
fn name_label(info: &SessionInfo) -> String {
    info.name.clone().unwrap_or_else(|| "-".to_owned())
}

/// Branch column value: the bound branch, or `-` for a plain session.
fn branch_label(info: &SessionInfo) -> String {
    info.branch.clone().unwrap_or_else(|| "-".to_owned())
}

/// Warning column value: a count, or `-` when there are none.
fn warn_count_label(info: &SessionInfo) -> String {
    if info.warnings.is_empty() {
        "-".to_owned()
    } else {
        info.warnings.len().to_string()
    }
}

fn subagent_count_label(info: &SessionInfo) -> String {
    if info.subagents.is_empty() {
        return "-".to_owned();
    }
    format!("{}/{}", running_subagent_count(info), info.subagents.len())
}

fn running_subagent_count(info: &SessionInfo) -> usize {
    info.subagents
        .iter()
        .filter(|subagent| subagent.lifecycle == SubagentLifecycle::Running)
        .count()
}

/// The inspect rows that summarize the background snapshot.
fn background_summary_rows(info: &SessionInfo, now_ms: u64) -> [(&'static str, String); 2] {
    let items = info.background.as_ref().map_or_else(
        || "<none>".to_owned(),
        |background| background.items.len().to_string(),
    );
    let observed = info.background.as_ref().map_or_else(
        || "<none>".to_owned(),
        |background| {
            format!(
                "{} ago",
                format_age(now_ms.saturating_sub(background.observed_at_ms))
            )
        },
    );
    [
        ("background_items", items),
        ("background_observed", observed),
    ]
}

/// Background column value: compact per-code counts in [`BACKGROUND_CODES`]
/// order. `-` means no snapshot has been reported; a valid empty snapshot
/// shows `0`, because it says no background task runs.
fn background_count_label(info: &SessionInfo) -> String {
    let Some(background) = info.background.as_ref() else {
        return "-".to_owned();
    };
    if background.items.is_empty() {
        return "0".to_owned();
    }
    let parts: Vec<String> = BACKGROUND_CODES
        .iter()
        .filter_map(|(kinds, code)| {
            let count = background
                .items
                .iter()
                .filter(|item| kinds.contains(&item.kind))
                .count();
            (count > 0).then(|| format!("{count}{code}"))
        })
        .collect();
    parts.join(" ")
}

/// Single-letter codes of the BACKGROUND column, in display order.
///
/// Each code covers the listed kinds. The `o` row lists every kind without a
/// letter of its own, so every snapshot item is counted exactly once.
const BACKGROUND_CODES: &[(&[protocol::BackgroundKind], char)] = &[
    (&[protocol::BackgroundKind::Subagent], 'a'),
    (&[protocol::BackgroundKind::Shell], 's'),
    (&[protocol::BackgroundKind::Monitor], 'm'),
    (&[protocol::BackgroundKind::Workflow], 'w'),
    (&[protocol::BackgroundKind::Teammate], 't'),
    (
        &[
            protocol::BackgroundKind::McpTask,
            protocol::BackgroundKind::Dream,
            protocol::BackgroundKind::AutoModeScan,
            protocol::BackgroundKind::MemoryImport,
            protocol::BackgroundKind::CloudSession,
            protocol::BackgroundKind::Other,
        ],
        'o',
    ),
];

fn origin_label(info: &SessionInfo) -> &'static str {
    if is_external_session(info) {
        "external"
    } else {
        "managed"
    }
}

fn cwd_source_label(source: Option<CwdSource>) -> &'static str {
    source.map_or("<none>", CwdSource::as_str)
}

fn is_external_session(info: &SessionInfo) -> bool {
    info.external == Some(true)
}

fn has_worktree_drift(info: &SessionInfo) -> bool {
    info.worktree_path
        .as_ref()
        .is_some_and(|worktree_path| !info.cwd.starts_with(worktree_path))
}

fn render_inspect_human(info: &SessionInfo) -> String {
    let none = || "<none>".to_owned();
    let mut rows: Vec<(&str, String)> = vec![
        ("id", info.id.0.clone()),
        ("external", yes_no(is_external_session(info)).to_owned()),
        ("read_only", yes_no(is_external_session(info)).to_owned()),
        ("name", info.name.clone().unwrap_or_else(&none)),
        ("agent", agent_label(&info.agent).to_owned()),
        ("cwd", info.cwd.display().to_string()),
        ("cwd_source", cwd_source_label(info.cwd_source).to_owned()),
        ("pid", info.pid.to_string()),
        ("cols", info.cols.to_string()),
        ("rows", info.rows.to_string()),
        ("state", state_label(info.state).to_owned()),
        ("activity", activity_label_option(info.activity).to_owned()),
        (
            "state_source",
            state_source_label(info.state_source).to_owned(),
        ),
        (
            "native_session_id",
            info.native_session_id.clone().unwrap_or_else(none),
        ),
        (
            "native_session_path",
            info.native_session_path.clone().unwrap_or_else(none),
        ),
        (
            "native_last_activity_at",
            info.native_last_activity_at.clone().unwrap_or_else(none),
        ),
        (
            "native_binding",
            if info.native_session_id.is_some() || info.native_session_path.is_some() {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
        ),
        (
            "repo",
            info.repo
                .as_ref()
                .map_or_else(none, |p| p.display().to_string()),
        ),
        ("branch", info.branch.clone().unwrap_or_else(none)),
        (
            "worktree_path",
            info.worktree_path
                .as_ref()
                .map_or_else(none, |p| p.display().to_string()),
        ),
        ("warnings", warn_count_label(info)),
        (
            "capability_refusals",
            info.capability_refusals
                .as_ref()
                .map_or_else(none, capability_refusals_label),
        ),
        (
            "subagents_running",
            running_subagent_count(info).to_string(),
        ),
        ("subagents_recent", info.subagents.len().to_string()),
        ("created_at", info.created_at.clone()),
        ("updated_at", info.updated_at.clone()),
        (
            "exit_code",
            info.exit_code.map_or_else(none, |code| code.to_string()),
        ),
    ];
    rows.extend(background_summary_rows(info, now_ms()));
    insert_active_agent_rows(&mut rows, info);
    if has_worktree_drift(info) {
        rows.push(("worktree_drift", "yes".to_owned()));
    }
    let width = rows
        .iter()
        .map(|(field, _)| field.len())
        .max()
        .unwrap_or(0)
        .max("FIELD".len());

    let mut output = String::new();
    let _ = writeln!(output, "{:<width$}  VALUE", "FIELD");
    for (field, value) in &rows {
        let _ = writeln!(output, "{field:<width$}  {value}");
    }
    append_inspect_details(&mut output, info, now_ms());
    output
}

/// Inserts the active nested-agent rows after the launch identity rows, in
/// their fixed display order, each only when the session reports it.
fn insert_active_agent_rows(rows: &mut Vec<(&'static str, String)>, info: &SessionInfo) {
    if let Some(active_agent) = &info.active_agent {
        rows.insert(3, ("active_agent", active_agent.clone()));
    }
    if let Some(active_base) = &info.active_agent_base {
        rows.insert(4, ("active_base", agent_kind_label(active_base).to_owned()));
    }
    if let Some(active_pid) = info.active_agent_pid {
        rows.insert(5, ("active_pid", active_pid.to_string()));
    }
    if let Some(active_id) = &info.active_agent_session_id {
        rows.insert(6, ("active_native_session_id", active_id.clone()));
    }
    if let Some(active_path) = &info.active_agent_session_path {
        rows.insert(7, ("active_native_session_path", active_path.clone()));
    }
}

fn append_inspect_details(output: &mut String, info: &SessionInfo, now_ms: u64) {
    // Each non-fatal warning is detailed below the table so a worktree session
    // surfaces exactly what happened and what was done instead.
    for warning in &info.warnings {
        let _ = writeln!(
            *output,
            "warning [{}]: {}",
            warning_kind_label(warning.kind),
            warning.message
        );
        if let Some(detail) = &warning.detail {
            let _ = writeln!(*output, "  detail: {detail}");
        }
    }
    let items: &[protocol::BackgroundItem] = info
        .background
        .as_ref()
        .map_or(&[], |background| background.items.as_slice());
    for subagent in &info.subagents {
        let kind = terminal_text(subagent.agent_type.as_deref().unwrap_or("agent"));
        let label = items
            .iter()
            .find(|item| is_subagent_item_of(item, &subagent.id))
            .and_then(|item| item.label.as_deref());
        let _ = write!(
            *output,
            "subagent [{}:{}]: {} {} started {} ago",
            agent_kind_label(&subagent.provider),
            terminal_text(&subagent.id),
            kind,
            subagent_lifecycle_label(subagent.lifecycle),
            format_age(now_ms.saturating_sub(subagent.started_at_ms)),
        );
        if let Some(label) = label {
            let _ = write!(*output, " label: {}", terminal_text(label));
        }
        let _ = writeln!(*output);
    }
    // A subagent item with a matching subagent row is shown on that row, so
    // only the remaining items get a background line.
    for item in items.iter().filter(|item| {
        !info
            .subagents
            .iter()
            .any(|subagent| is_subagent_item_of(item, &subagent.id))
    }) {
        let program = item
            .program
            .as_deref()
            .map_or_else(|| "-".to_owned(), terminal_text);
        let label = item
            .label
            .as_deref()
            .map_or_else(|| "-".to_owned(), terminal_text);
        let _ = writeln!(
            *output,
            "background [{}]: {} {} {} first seen >= {}",
            background_kind_label(item),
            background_status_label(item.status),
            program,
            label,
            format_age(now_ms.saturating_sub(item.first_seen_ms)),
        );
    }
}

/// Whether a snapshot item describes the subagent row with `subagent_id`.
fn is_subagent_item_of(item: &protocol::BackgroundItem, subagent_id: &str) -> bool {
    item.kind == protocol::BackgroundKind::Subagent && item.id == subagent_id
}

/// Kind column of an inspect background line; provider strings outside the
/// closed kind set are shown after a colon.
fn background_kind_label(item: &protocol::BackgroundItem) -> String {
    let kind = match item.kind {
        protocol::BackgroundKind::Subagent => "subagent",
        protocol::BackgroundKind::Workflow => "workflow",
        protocol::BackgroundKind::Shell => "shell",
        protocol::BackgroundKind::Monitor => "monitor",
        protocol::BackgroundKind::McpTask => "mcp_task",
        protocol::BackgroundKind::Teammate => "teammate",
        protocol::BackgroundKind::Dream => "dream",
        protocol::BackgroundKind::AutoModeScan => "auto_mode_scan",
        protocol::BackgroundKind::MemoryImport => "memory_import",
        protocol::BackgroundKind::CloudSession => "cloud_session",
        protocol::BackgroundKind::Other => "other",
    };
    match (&item.kind, &item.provider_kind) {
        (protocol::BackgroundKind::Other, Some(provider_kind)) => {
            format!("{kind}:{}", terminal_text(provider_kind))
        }
        _ => kind.to_owned(),
    }
}

const fn background_status_label(status: protocol::BackgroundStatus) -> &'static str {
    match status {
        protocol::BackgroundStatus::Running => "running",
        protocol::BackgroundStatus::Pending => "pending",
        protocol::BackgroundStatus::Other => "other",
    }
}

/// Milliseconds per second, the base of the compact age labels.
const MS_PER_SECOND: u64 = 1_000;
/// Compact age label boundaries: a label switches to the next unit at these
/// counts, so `59s` is followed by `1m`. Only display; no protocol value
/// depends on them.
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3_600;
const SECONDS_PER_DAY: u64 = 86_400;

/// Compact age of an elapsed duration in milliseconds, such as `45s`, `3m`,
/// `2h` or `1d`.
fn format_age(age_ms: u64) -> String {
    let seconds = age_ms / MS_PER_SECOND;
    if seconds < SECONDS_PER_MINUTE {
        format!("{seconds}s")
    } else if seconds < SECONDS_PER_HOUR {
        format!("{}m", seconds / SECONDS_PER_MINUTE)
    } else if seconds < SECONDS_PER_DAY {
        format!("{}h", seconds / SECONDS_PER_HOUR)
    } else {
        format!("{}d", seconds / SECONDS_PER_DAY)
    }
}

/// Current Unix time in milliseconds; 0 when the clock is before the epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Escape a provider-supplied text value for a terminal line.
///
/// Labels and programs come from a daemon that may be older, newer or remote,
/// so each unsafe code point is printed as its `\u{...}` escape.
fn terminal_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if pohunek_worker_protocol::is_hidden_or_separator(character) {
            output.extend(character.escape_unicode());
        } else {
            output.push(character);
        }
    }
    output
}

const fn subagent_lifecycle_label(lifecycle: SubagentLifecycle) -> &'static str {
    match lifecycle {
        SubagentLifecycle::Running => "running",
        SubagentLifecycle::Completed => "completed",
        SubagentLifecycle::Failed => "failed",
        SubagentLifecycle::Cancelled => "cancelled",
        SubagentLifecycle::Lost => "lost",
        SubagentLifecycle::Unknown => "unknown",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn render_stop_human(session_id: &str, result: &SessionStopResult) -> String {
    format!("session {session_id}: stopped={}\n", result.stopped)
}

/// Append `warnings` to `output`, one `warning [kind]: message` line each.
fn write_warnings(output: &mut String, warnings: &[protocol::SessionWarning]) {
    for warning in warnings {
        let _ = writeln!(
            output,
            "  warning [{}]: {}",
            warning_kind_label(warning.kind),
            warning.message
        );
        // A failing hook's stderr tail is only in this result, so its
        // multi-line detail is shown here, indented under the warning.
        if let Some(detail) = &warning.detail {
            let mut lines = detail.lines();
            if let Some(first) = lines.next() {
                let _ = writeln!(output, "    detail: {first}");
            }
            for line in lines {
                let _ = writeln!(output, "      {line}");
            }
        }
    }
}

fn render_remove_human(session_id: &str, result: &SessionRemoveResult) -> String {
    let mut output = format!(
        "session {session_id}: removed={} stopped={}\n",
        result.removed, result.stopped
    );
    // Worktree cleanup is best-effort inside the daemon, so a leftover checkout
    // is only visible if it is printed here.
    if result.worktrees_removed > 0 || result.worktrees_failed > 0 {
        let _ = writeln!(
            output,
            "  worktrees: removed={} failed={}",
            result.worktrees_removed, result.worktrees_failed
        );
    }
    // Each accepted process is listed so the operator can inspect it: it was
    // not signalled and may still be running unsupervised.
    if !result.accepted_unconfirmed_processes.is_empty() {
        output
            .push_str("  accepted unconfirmed processes (not signalled, may still be running):\n");
        for process in &result.accepted_unconfirmed_processes {
            let _ = write!(
                output,
                "    pid {} (start {}",
                process.pid, process.start_identity
            );
            if let Some(command) = &process.command {
                let _ = write!(output, ", command {command}");
            }
            output.push_str(")\n");
        }
    }
    write_warnings(&mut output, &result.warnings);
    output
}

fn render_input_human(session_id: &str, result: &SessionInputResult) -> String {
    let disposition = if result.accepted {
        "accepted"
    } else {
        "rejected"
    };
    let submitted = if result.submitted == Some(true) {
        " submitted=true"
    } else {
        ""
    };
    match (result.activity, result.activity_source) {
        (Some(activity), Some(source)) => format!(
            "session {session_id}: input {disposition}{submitted} activity={} source={}\n",
            activity_label(activity),
            state_source_label(source)
        ),
        _ => format!("session {session_id}: input {disposition}\n"),
    }
}

fn render_rename_human(info: &SessionInfo) -> String {
    match &info.name {
        Some(name) => format!("session {}: renamed to {name:?}\n", info.id.0),
        None => format!("session {}: name cleared\n", info.id.0),
    }
}

/// The agent name as displayed. Free-string since Part C, so the label is the name
/// itself (kept as a function so the call sites read uniformly with the other
/// `*_label` helpers).
fn agent_label(agent: &str) -> &str {
    agent
}

fn session_agent_label(info: &SessionInfo) -> String {
    match info.active_agent.as_deref() {
        Some(active_agent) if active_agent != info.agent => {
            format!("{}->{active_agent}", info.agent)
        }
        _ => agent_label(&info.agent).to_owned(),
    }
}

fn agent_kind_label(agent: &protocol::RuntimeRef) -> &str {
    agent.as_wire()
}

fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Starting => "starting",
        SessionState::Running => "running",
        SessionState::Stopped => "stopped",
        SessionState::Done => "done",
        SessionState::Failed => "failed",
    }
}

fn activity_label_option(activity: Option<AgentActivity>) -> &'static str {
    activity.map_or("-", activity_label)
}

fn activity_label(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "blocked",
        AgentActivity::Idle => "idle",
        AgentActivity::Unknown => "unknown",
    }
}

fn state_source_label(source: StateSource) -> &'static str {
    match source {
        StateSource::OscTitle => "osc_title",
        StateSource::OscProgress => "osc_progress",
        StateSource::Screen => "screen",
        StateSource::Process => "process",
        StateSource::Report => "report",
        StateSource::Unknown => "unknown",
    }
}

/// The operations a session's recovery resolution refuses, with the error code
/// each returns for it, as `resume=<code> fork=<code>`.
fn capability_refusals_label(refusals: &protocol::SessionCapabilityRefusals) -> String {
    [("resume", &refusals.resume), ("fork", &refusals.fork)]
        .into_iter()
        .filter_map(|(operation, code)| code.as_ref().map(|code| format!("{operation}={code}")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn warning_kind_label(kind: SessionWarningKind) -> &'static str {
    match kind {
        SessionWarningKind::Fetch => "fetch",
        SessionWarningKind::BaseBranchFallback => "base_branch_fallback",
        SessionWarningKind::SetupScript => "setup_script",
        SessionWarningKind::Hook => "hook",
        SessionWarningKind::NativeRecovery => "native_recovery",
        SessionWarningKind::BranchAheadOfRemote => "branch_ahead_of_remote",
        SessionWarningKind::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use protocol::SessionWarning;
    use serde_json::json;

    use super::*;
    use crate::target::LOCAL_HOST;

    fn running_session(id: &str) -> SessionInfo {
        SessionInfo {
            name: None,
            id: protocol::SessionId(id.to_owned()),
            external: Some(false),
            capabilities: protocol::SessionCapabilities::default(),
            agent: "shell".to_owned(),
            agent_base: protocol::RuntimeRef::shell(),
            cwd: PathBuf::from("/workspace/project"),
            cwd_source: Some(protocol::CwdSource::Launch),
            pid: 4242,
            cols: 120,
            rows: 40,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            background: None,
            native_session_id: None,
            native_session_path: None,
            native_last_activity_at: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            project_id: None,
            project_label: None,
            metadata: std::collections::BTreeMap::new(),
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            capability_refusals: None,
            output_force_closed: false,
            created_at: "2026-06-17T10:00:00Z".to_owned(),
            updated_at: "2026-06-17T10:01:00Z".to_owned(),
            exit_code: None,
            runtime: None,
        }
    }

    fn new_args(agent: &str, cwd: Option<PathBuf>) -> NewArgs {
        NewArgs {
            agent: agent.to_owned(),
            name: None,
            cwd,
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            from_remote: false,
            reset_diverged: false,
            input: None,
            input_ack_timeout_ms: None,
            request_timeout_ms: None,
            meta: Vec::new(),
        }
    }

    #[test]
    fn read_parsers_accept_unwrapped_aliases_and_bound_lines() {
        assert_eq!(
            parse_read_source("recent_unwrapped").expect("wire-style source"),
            SessionReadSource::RecentUnwrapped
        );
        assert_eq!(
            parse_read_source("recent-unwrapped").expect("CLI-style alias"),
            SessionReadSource::RecentUnwrapped
        );
        let error = parse_read_source("unknown").expect_err("unknown source");
        assert!(error.contains("recent_unwrapped"));
        assert!(error.contains("recent-unwrapped"));
        parse_read_lines("0").expect_err("zero lines must fail");
        assert_eq!(parse_read_lines("1").expect("minimum lines"), 1);
        let ceiling = MAX_SESSION_READ_LINES.to_string();
        assert_eq!(
            parse_read_lines(&ceiling).expect("maximum lines"),
            MAX_SESSION_READ_LINES
        );
        parse_read_lines(&(MAX_SESSION_READ_LINES + 1).to_string())
            .expect_err("lines above the protocol ceiling must fail");
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "test helper takes the json! literal by value to keep call sites terse"
    )]
    fn assert_request(request: &Request, method_name: &str, params: serde_json::Value) {
        assert_eq!(request.version_range(), protocol::CLIENT_PROTOCOL_VERSIONS);
        assert_eq!(request.method(), method_name, "method");
        assert_eq!(request.params(), &params, "params");
        // The id is a unique per-call SDK correlation id; assert only its
        // stable, log-greppable `sdk-<method>-` prefix.
        assert!(
            request.id().starts_with(&format!("sdk-{method_name}-")),
            "id {:?} must be prefixed by the method",
            request.id()
        );
    }

    #[test]
    fn new_request_defaults_to_shell_size_and_omits_cwd() {
        let request = build_new_request(&new_args("shell", None)).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "shell",
                "cols": 80,
                "rows": 24
            }),
        );
    }

    #[test]
    fn parse_meta_pair_splits_on_the_first_equals_and_rejects_malformed_pairs() {
        // A value that itself contains '=' (a URL query string) stays whole.
        assert_eq!(
            parse_meta_pair("link.url=https://example.com?a=1&b=2").expect("value with ="),
            (
                "link.url".to_owned(),
                "https://example.com?a=1&b=2".to_owned()
            )
        );
        for (input, expected) in [
            ("link.provider", "expected key=value"),
            ("=github", "key cannot be empty"),
        ] {
            let err = parse_meta_pair(input).expect_err(input);
            assert!(err.contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn new_request_carries_every_optional_field() {
        let args = NewArgs {
            agent: "claude".to_owned(),
            name: None,
            cwd: Some(PathBuf::from("/workspace/cwd")),
            cols: 120,
            rows: 40,
            project: Some("ui".to_owned()),
            repo: Some(PathBuf::from("/workspace/repo")),
            branch: Some("feature/login".to_owned()),
            base_branch: Some("main".to_owned()),
            from_remote: false,
            reset_diverged: false,
            input: Some("Fix #1234".to_owned()),
            input_ack_timeout_ms: None,
            request_timeout_ms: None,
            meta: vec![
                ("link.provider".to_owned(), "github".to_owned()),
                ("link.kind".to_owned(), "pull_request".to_owned()),
            ],
        };
        let request = build_new_request(&args).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "claude",
                "cwd": "/workspace/cwd",
                "cols": 120,
                "rows": 40,
                "project": "ui",
                "repo": "/workspace/repo",
                "branch": "feature/login",
                "base_branch": "main",
                "input": "Fix #1234",
                "metadata": {
                    "link.kind": "pull_request",
                    "link.provider": "github"
                }
            }),
        );
    }

    #[test]
    fn list_request_sends_typed_filters() {
        let request = build_list_request(&[
            parse_list_filter("state=running").expect("state filter"),
            parse_list_filter("agent=claude").expect("agent filter"),
            parse_list_filter("id=s-42").expect("id filter"),
        ])
        .expect("request");

        assert_request(
            &request,
            method::SESSION_LIST,
            json!({
                "filters": [
                    { "key": "state", "value": "running" },
                    { "key": "agent", "value": "claude" },
                    { "key": "id", "value": "s-42" }
                ]
            }),
        );
    }

    #[test]
    fn list_filter_matches_active_agent_identity() {
        let mut session = running_session("s-42");
        session.active_agent = Some("codex".to_owned());
        session.active_agent_base = Some(protocol::RuntimeRef::codex());

        assert!(
            parse_list_filter("agent=codex")
                .expect("active agent")
                .matches(&session),
            "active agent should match the client-side fallback filter"
        );
        assert!(
            parse_list_filter("agent=shell")
                .expect("launch agent")
                .matches(&session),
            "launch agent should remain filterable"
        );
    }

    #[test]
    fn list_filters_are_anded() {
        let mut codex = running_session("s-codex");
        codex.agent = "codex".to_owned();
        codex.activity = Some(AgentActivity::Working);
        let filters = vec![
            parse_list_filter("state=running").expect("state filter"),
            parse_list_filter("agent=codex").expect("agent filter"),
            parse_list_filter("activity=working").expect("activity filter"),
        ];

        assert!(session_matches_filters(&codex, &filters));
        assert!(!session_matches_filters(
            &running_session("s-shell"),
            &filters
        ));
    }

    #[test]
    fn client_and_protocol_filter_predicates_agree() {
        // The client re-filters what the daemon already filtered, using a second
        // predicate (`ListFilter::matches`). It MUST agree with the predicate the
        // daemon runs (`protocol::SessionListFilter::matches`) for every variant
        // and outcome, or the defense-in-depth re-filter could narrow the set
        // differently from the daemon. Pin them together so they cannot drift.
        let mut codex = running_session("s-codex");
        codex.agent = "codex".to_owned();
        codex.activity = Some(AgentActivity::Working);
        codex.project_id = Some("p-aaa".to_owned());
        codex.project_label = Some("ui".to_owned());
        let sessions = [running_session("s-42"), codex];

        let filters = [
            parse_list_filter("state=running").expect("state"),
            parse_list_filter("state=stopped").expect("state"),
            parse_list_filter("agent=claude").expect("agent"),
            parse_list_filter("agent=codex").expect("agent"),
            parse_list_filter("activity=working").expect("activity"),
            parse_list_filter("activity=blocked").expect("activity"),
            parse_list_filter("id=s-42").expect("id"),
            parse_list_filter("id=s-codex").expect("id"),
            parse_list_filter("id=s-4").expect("id"),
            parse_list_filter("project=ui").expect("project label"),
            parse_list_filter("project=p-aaa").expect("project id"),
            parse_list_filter("project=nope").expect("project miss"),
        ];

        for session in &sessions {
            for filter in &filters {
                assert_eq!(
                    filter.matches(session),
                    filter.to_protocol_filter().matches(session),
                    "client and protocol predicates disagree for {filter:?} on {:?}",
                    session.id
                );
            }
        }
    }

    #[test]
    fn project_filter_matches_by_label_or_id_and_column_falls_back() {
        let mut session = running_session("s-42");
        session.project_id = Some("p-abc123".to_owned());
        session.project_label = Some("ui".to_owned());

        // The PROJECT column prefers the label.
        assert_eq!(project_label(&session), "ui");
        // A project filter matches by label OR by id.
        assert!(parse_list_filter("project=ui")
            .expect("label")
            .matches(&session));
        assert!(parse_list_filter("project=p-abc123")
            .expect("id")
            .matches(&session));
        assert!(!parse_list_filter("project=other")
            .expect("miss")
            .matches(&session));

        // With the label unresolved, the column falls back to the id; a no-project
        // session shows `-` and matches no project filter.
        session.project_label = None;
        assert_eq!(project_label(&session), "p-abc123");
        let plain = running_session("s-plain");
        assert_eq!(project_label(&plain), "-");
        assert!(!parse_list_filter("project=ui").expect("f").matches(&plain));
    }

    #[test]
    fn json_and_quiet_list_outputs_use_the_same_filtered_sessions() {
        let mut codex = running_session("s-codex");
        codex.agent = "codex".to_owned();
        let mut claude = running_session("s-claude");
        claude.agent = "claude".to_owned();
        claude.state = SessionState::Stopped;
        let sessions = vec![running_session("s-shell"), codex, claude];
        let filters = vec![parse_list_filter("state=running").expect("filter")];

        let json_output =
            render_list_output(&sessions, &filters, ListOutputMode::Json).expect("json output");
        let quiet_output =
            render_list_output(&sessions, &filters, ListOutputMode::Quiet).expect("quiet output");
        let json_ids: Vec<String> =
            crate::commands::parse_json_ok::<Vec<SessionInfo>>(&json_output)
                .into_iter()
                .map(|session| session.id.0)
                .collect();
        let quiet_ids: Vec<&str> = quiet_output.lines().collect();

        assert_eq!(quiet_ids, json_ids);
        assert_eq!(json_ids, vec!["s-shell", "s-codex"]);
    }

    #[test]
    fn input_request_extracts_session_id_regardless_of_host() {
        // The request carries only the session id; host routing is the
        // transport's job, so a remote target's host never enters the body.
        let remote: Target = "host-b/s-42".parse().expect("target");
        let request = build_input_request(&remote, "write tests first").expect("request");

        assert_request(
            &request,
            method::SESSION_INPUT,
            json!({
                "session_id": "s-42",
                "text": "write tests first"
            }),
        );
    }

    #[test]
    fn rename_request_sends_the_name_or_omits_it_to_clear() {
        let target: Target = "host-b/s-42".parse().expect("target");
        let request =
            build_rename_request(&target, Some("triage build".to_owned())).expect("request");
        assert_request(
            &request,
            method::SESSION_RENAME,
            json!({"session_id": "s-42", "name": "triage build"}),
        );

        // The daemon treats an absent name as clear.
        let request = build_rename_request(&target, None).expect("request");
        assert_request(
            &request,
            method::SESSION_RENAME,
            json!({"session_id": "s-42"}),
        );
    }

    #[test]
    fn diff_request_sends_an_explicit_base_or_leaves_it_to_the_daemon() {
        let target: Target = "host-b/s-42".parse().expect("target");
        let request = build_diff_request(&target, Some("main".to_owned())).expect("request");
        assert_request(
            &request,
            method::SESSION_DIFF,
            json!({"session_id": "s-42", "base": "main"}),
        );

        // Without `--base` the daemon resolves the default base itself.
        let request = build_diff_request(&target, None).expect("request");
        assert_request(
            &request,
            method::SESSION_DIFF,
            json!({"session_id": "s-42"}),
        );
    }

    #[test]
    fn prepare_new_args_local_respects_explicit_cwd() {
        let prepared = prepare_new_args(
            LOCAL_HOST,
            new_args("shell", Some(PathBuf::from("/explicit"))),
        )
        .expect("local prepare succeeds");
        assert_eq!(prepared.cwd, Some(PathBuf::from("/explicit")));
    }

    #[test]
    fn prepare_new_args_remote_without_project_or_repo_is_rejected() {
        // The CLI's own directory means nothing on a remote host: a remote start
        // must name a --project (or --repo), and is rejected before any dial.
        let err = prepare_new_args("host-b", new_args("shell", Some(PathBuf::from("/x"))))
            .expect_err("remote without a target is rejected");
        assert!(
            matches!(err, CliError::RemoteTargetRequired),
            "expected RemoteTargetRequired, got {err:?}"
        );
    }

    #[test]
    fn prepare_new_args_remote_with_repo_is_allowed() {
        let mut args = new_args("shell", None);
        args.repo = Some(PathBuf::from("/on/remote"));
        let prepared = prepare_new_args("host-b", args).expect("remote with --repo");
        assert_eq!(prepared.cwd, None);
        assert_eq!(prepared.repo, Some(PathBuf::from("/on/remote")));
    }

    #[test]
    fn confirmation_gate_local_always_proceeds() {
        // Local `session new` is never gated, regardless of json/yes.
        for json in [false, true] {
            for yes in [false, true] {
                assert_eq!(
                    confirmation_decision(LOCAL_HOST, json, yes),
                    ConfirmDecision::Proceed,
                    "local must proceed (json={json}, yes={yes})"
                );
                assert_eq!(
                    confirmation_decision("", json, yes),
                    ConfirmDecision::Proceed,
                    "empty host (implicit local) must proceed"
                );
            }
        }
    }

    #[test]
    fn confirmation_gate_remote_requires_yes_on_json_path() {
        // Machine path (json) without --yes must fail fast, never block on a
        // prompt.
        assert_eq!(
            confirmation_decision("host-b", true, false),
            ConfirmDecision::RequireYes
        );
        // With --yes the machine path proceeds.
        assert_eq!(
            confirmation_decision("host-b", true, true),
            ConfirmDecision::Proceed
        );
    }

    #[test]
    fn confirmation_gate_remote_prompts_on_human_path() {
        // Human path without --yes prompts; with --yes it proceeds.
        assert_eq!(
            confirmation_decision("host-b", false, false),
            ConfirmDecision::Prompt
        );
        assert_eq!(
            confirmation_decision("host-b", false, true),
            ConfirmDecision::Proceed
        );
    }

    /// Whitespace-split tokens of the list row whose first column is `id`.
    /// Robust to column-width changes (the table is space-padded).
    fn list_row(output: &str, id: &str) -> Vec<String> {
        output
            .lines()
            .find(|line| line.split_whitespace().next() == Some(id))
            .map(|line| line.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    #[test]
    fn renders_compact_session_list_table() {
        let output = render_list_human(&[running_session("s-42")]);

        let header = output.lines().next().expect("header line");
        for column in [
            "ID",
            "NAME",
            "AGENT",
            "ORIGIN",
            "STATE",
            "PROJECT",
            "BRANCH",
            "WARN",
            "SUBAGENTS",
            "BACKGROUND",
            "CWD",
        ] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        // Columns: ID NAME AGENT ORIGIN STATE ACTIVITY SOURCE PID SIZE PROJECT BRANCH WARN SUBAGENTS BACKGROUND CWD.
        assert_eq!(
            list_row(&output, "s-42"),
            vec![
                "s-42",
                "-", // NAME
                "shell",
                "managed",
                "running",
                "-",
                "process",
                "4242",
                "120x40",
                "-", // PROJECT
                "-", // BRANCH
                "-", // WARN
                "-", // SUBAGENTS
                "-", // BACKGROUND
                "/workspace/project",
            ]
        );
    }

    #[test]
    fn renders_active_agent_in_session_list_table() {
        let mut session = running_session("s-42");
        session.active_agent = Some("codex".to_owned());
        session.active_agent_base = Some(protocol::RuntimeRef::codex());

        let output = render_list_human(&[session]);

        assert_eq!(
            list_row(&output, "s-42")[2],
            "shell->codex",
            "AGENT column should show launch and active agent"
        );
    }

    #[test]
    fn renders_worktree_branch_and_warning_count_in_list() {
        let mut session = running_session("s-42");
        session.cwd = PathBuf::from("/data/worktrees/s-42-project-feature-login");
        session.branch = Some("feature/login".to_owned());
        session.worktree_path = Some(session.cwd.clone());
        session.warnings = vec![SessionWarning {
            kind: SessionWarningKind::Fetch,
            message: "fetch failed".to_owned(),
            detail: None,
        }];

        session.project_id = Some("p-abc123".to_owned());
        session.project_label = Some("login-ui".to_owned());

        let output = render_list_human(&[session]);
        let row = list_row(&output, "s-42");
        // Columns: ID NAME AGENT ORIGIN STATE ACTIVITY SOURCE PID SIZE PROJECT BRANCH WARN SUBAGENTS BACKGROUND CWD.
        assert_eq!(
            row[9], "login-ui",
            "project column shows the label: {row:?}"
        );
        assert_eq!(row[10], "feature/login", "branch column: {row:?}");
        assert_eq!(row[11], "1", "warning-count column: {row:?}");
        assert_eq!(
            row[14], "/data/worktrees/s-42-project-feature-login",
            "cwd is the worktree path: {row:?}"
        );
    }

    #[test]
    fn renders_subagent_summary_in_list_and_details_in_inspect() {
        let mut session = running_session("s-42");
        session.subagents = vec![
            protocol::SubagentInfo {
                id: "child-running".to_owned(),
                parent_id: None,
                provider: protocol::RuntimeRef::claude(),
                agent_type: Some("Explore".to_owned()),
                lifecycle: protocol::SubagentLifecycle::Running,
                activity: Some(AgentActivity::Working),
                revision: protocol::SubagentRevision::new(1),
                started_at_ms: 100,
                updated_at_ms: 100,
                finished_at_ms: None,
            },
            protocol::SubagentInfo {
                id: "child-done".to_owned(),
                parent_id: None,
                provider: protocol::RuntimeRef::codex(),
                agent_type: None,
                lifecycle: protocol::SubagentLifecycle::Completed,
                activity: None,
                revision: protocol::SubagentRevision::new(2),
                started_at_ms: 90,
                updated_at_ms: 110,
                finished_at_ms: Some(110),
            },
        ];

        let list = render_list_human(&[session.clone()]);
        assert_eq!(list_row(&list, "s-42")[12], "1/2");

        let inspect = render_inspect_human(&session);
        assert!(has_row(&inspect, "subagents_running", "1"));
        assert!(has_row(&inspect, "subagents_recent", "2"));
        assert!(inspect.contains("subagent [claude:child-running]: Explore running started "));
        assert!(inspect.contains("subagent [codex:child-done]: agent completed started "));
    }

    /// Whether the rendered field/value table contains a `field value` row,
    /// tolerant of the column width (which the longest field name sets).
    fn has_row(output: &str, field: &str, value: &str) -> bool {
        output.lines().any(|line| {
            let mut parts = line.split_whitespace();
            parts.next() == Some(field) && parts.next() == Some(value) && parts.next().is_none()
        })
    }

    #[test]
    fn renders_inspect_field_value_table() {
        let output = render_inspect_human(&running_session("s-42"));

        assert!(has_row(&output, "FIELD", "VALUE"));
        assert!(has_row(&output, "id", "s-42"));
        assert!(has_row(&output, "external", "no"));
        assert!(has_row(&output, "read_only", "no"));
        assert!(has_row(&output, "agent", "shell"));
        assert!(has_row(&output, "state", "running"));
        assert!(has_row(&output, "activity", "-"));
        assert!(has_row(&output, "state_source", "process"));
        assert!(has_row(&output, "native_session_id", "<none>"));
        assert!(has_row(&output, "native_binding", "no"));
        assert!(has_row(&output, "exit_code", "<none>"));
        assert!(has_row(&output, "repo", "<none>"));
        assert!(has_row(&output, "branch", "<none>"));
        assert!(has_row(&output, "worktree_path", "<none>"));
        assert!(has_row(&output, "warnings", "-"));
    }

    #[test]
    fn renders_external_read_only_in_inspect_table() {
        let mut session = running_session("ext-42");
        session.external = Some(true);

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "external", "yes"));
        assert!(has_row(&output, "read_only", "yes"));
    }

    #[test]
    fn renders_native_session_id_and_binding_when_captured() {
        let mut session = running_session("s-42");
        session.native_session_id = Some("native-abc".to_owned());

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "native_session_id", "native-abc"));
        assert!(has_row(&output, "native_binding", "yes"));
    }

    #[test]
    fn terminal_session_still_shows_captured_native_binding() {
        for state in [
            SessionState::Stopped,
            SessionState::Done,
            SessionState::Failed,
        ] {
            let mut session = running_session("s-42");
            session.native_session_id = Some("native-abc".to_owned());
            session.state = state;

            let output = render_inspect_human(&session);

            assert!(
                has_row(&output, "native_session_id", "native-abc"),
                "native id stays visible in state {state:?}: {output}"
            );
            assert!(
                has_row(&output, "native_binding", "yes"),
                "the native binding remains visible in state {state:?}: {output}"
            );
        }
    }

    #[test]
    fn renders_active_agent_fields_in_session_inspect_table() {
        let mut session = running_session("s-42");
        session.active_agent = Some("codex".to_owned());
        session.active_agent_base = Some(protocol::RuntimeRef::codex());
        session.active_agent_pid = Some(9001);
        session.active_agent_session_id = Some("codex-native".to_owned());
        session.active_agent_session_path = Some("/work/codex/session.json".to_owned());

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "agent", "shell"));
        assert!(has_row(&output, "active_agent", "codex"));
        assert!(has_row(&output, "active_base", "codex"));
        assert!(has_row(&output, "active_pid", "9001"));
        assert!(has_row(&output, "active_native_session_id", "codex-native"));
        assert!(has_row(
            &output,
            "active_native_session_path",
            "/work/codex/session.json"
        ));
        assert!(has_row(&output, "native_session_id", "<none>"));
    }

    #[test]
    fn list_spells_out_the_recovery_hint_of_a_native_recovery_warning() {
        let mut session = running_session("s-lost");
        session.warnings = vec![SessionWarning {
            kind: SessionWarningKind::NativeRecovery,
            message: "native recovery is unavailable: start a new session".to_owned(),
            detail: Some("native session id: native-1".to_owned()),
        }];
        let clean = running_session("s-clean");

        let output = render_list_human(&[session, clean]);

        assert!(
            output.contains(
                "warning [native_recovery] s-lost: native recovery is unavailable: start a new session"
            ),
            "list must carry the recovery hint: {output}"
        );
        assert!(!output.contains("s-clean:"), "{output}");
    }

    #[test]
    fn renders_worktree_fields_and_warning_detail_in_inspect() {
        let mut session = running_session("s-42");
        session.repo = Some(PathBuf::from("/workspace/project"));
        session.branch = Some("feature/login".to_owned());
        session.worktree_path = Some(PathBuf::from("/data/worktrees/s-42-project-feature-login"));
        session.cwd = session.worktree_path.clone().expect("worktree path");
        session.warnings = vec![SessionWarning {
            kind: SessionWarningKind::BaseBranchFallback,
            message: "Requested base branch \"release\" not found; used \"main\".".to_owned(),
            detail: Some("git rev-parse failed".to_owned()),
        }];

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "repo", "/workspace/project"));
        assert!(has_row(&output, "branch", "feature/login"));
        assert!(has_row(
            &output,
            "worktree_path",
            "/data/worktrees/s-42-project-feature-login"
        ));
        assert!(has_row(&output, "warnings", "1"));
        assert!(
            output.contains("warning [base_branch_fallback]:"),
            "inspect must detail the warning: {output}"
        );
        assert!(
            output.contains("detail: git rev-parse failed"),
            "inspect must show warning detail: {output}"
        );
    }

    #[test]
    fn renders_cwd_source_and_worktree_drift_in_inspect() {
        let mut session = running_session("s-42");
        session.cwd_source = Some(protocol::CwdSource::Osc7);
        session.worktree_path = Some(PathBuf::from("/data/worktrees/s-42-project-feature-login"));
        session.cwd = PathBuf::from("/workspace/project");

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "cwd_source", "osc7"));
        assert!(has_row(&output, "worktree_drift", "yes"));
    }

    #[test]
    fn renders_new_summary_with_worktree_and_warnings() {
        let mut session = running_session("s-42");
        session.branch = Some("feature/login".to_owned());
        session.worktree_path = Some(PathBuf::from("/data/worktrees/s-42-project-feature-login"));
        session.cwd = session.worktree_path.clone().expect("worktree path");
        session.warnings = vec![
            SessionWarning {
                kind: SessionWarningKind::SetupScript,
                message: "Repository setup script failed; the worktree was kept without it."
                    .to_owned(),
                detail: None,
            },
            SessionWarning {
                kind: SessionWarningKind::Hook,
                message: "The post-create hook failed; the session proceeded without it."
                    .to_owned(),
                detail: Some(
                    "/repo/.pohunek/hooks/post-create exited with status exit status: 7; \
                     stderr tail:\n[12 earlier bytes omitted]\nnpm ERR! install failed"
                        .to_owned(),
                ),
            },
        ];

        let output = render_new_human(&session, None, None);

        assert!(
            output.starts_with("session s-42 created (state: running)\n"),
            "{output}"
        );
        assert!(
            output.contains(
                "worktree: /data/worktrees/s-42-project-feature-login (branch feature/login)"
            ),
            "new summary must mention the worktree: {output}"
        );
        assert!(
            output.contains("warning [setup_script]:"),
            "new summary must mention warnings: {output}"
        );
        assert!(
            output.contains(
                "    detail: /repo/.pohunek/hooks/post-create exited with status exit status: 7; stderr tail:\n      [12 earlier bytes omitted]\n      npm ERR! install failed\n"
            ),
            "new summary must show the hook's stderr tail: {output}"
        );
    }

    #[test]
    fn renders_input_result_with_target_id() {
        let output = render_input_human(
            "s-42",
            &protocol::SessionInputResult {
                accepted: true,
                activity: None,
                activity_source: None,
                runtime: None,
                activity_epoch: None,
                activity_revision: None,
                submitted: None,
            },
        );

        assert_eq!(output, "session s-42: input accepted\n");
    }

    #[test]
    fn renders_settled_input_activity_and_source() {
        let output = render_input_human(
            "s-42",
            &protocol::SessionInputResult {
                accepted: true,
                activity: Some(AgentActivity::Blocked),
                activity_source: Some(StateSource::Report),
                runtime: Some(
                    protocol::SessionRuntimeIdentity::new(
                        "runtime-42",
                        protocol::RuntimeGeneration::new(1),
                    )
                    .expect("valid runtime identity"),
                ),
                activity_epoch: Some("d-epoch-1".to_owned()),
                activity_revision: Some(protocol::ActivityRevision::new(2)),
                submitted: None,
            },
        );

        assert_eq!(
            output,
            "session s-42: input accepted activity=blocked source=report\n"
        );
    }

    #[test]
    fn renders_stop_result_with_target_id() {
        let output = render_stop_human("s-42", &protocol::SessionStopResult { stopped: true });

        assert_eq!(output, "session s-42: stopped=true\n");
    }

    #[test]
    fn renders_remove_result_lists_every_accepted_unconfirmed_process() {
        let output = render_remove_human(
            "s-42",
            &SessionRemoveResult {
                removed: true,
                stopped: false,
                worktrees_removed: 0,
                worktrees_failed: 0,
                accepted_unconfirmed_processes: vec![
                    protocol::UnconfirmedProcess {
                        pid: 4242,
                        start_identity: protocol::ProcessStartIdentity::new(99),
                        command: Some("gpg-agent".to_owned()),
                    },
                    protocol::UnconfirmedProcess {
                        pid: 7,
                        start_identity: protocol::ProcessStartIdentity::new(3),
                        command: None,
                    },
                ],
                warnings: Vec::new(),
            },
        );

        assert_eq!(
            output,
            concat!(
                "session s-42: removed=true stopped=false\n",
                "  accepted unconfirmed processes (not signalled, may still be running):\n",
                "    pid 4242 (start 99, command gpg-agent)\n",
                "    pid 7 (start 3)\n"
            )
        );
    }

    #[test]
    fn renders_remove_result_worktree_cleanup_failure() {
        let output = render_remove_human(
            "s-42",
            &SessionRemoveResult {
                removed: true,
                stopped: false,
                worktrees_removed: 1,
                worktrees_failed: 2,
                accepted_unconfirmed_processes: Vec::new(),
                warnings: Vec::new(),
            },
        );

        assert_eq!(
            output,
            "session s-42: removed=true stopped=false\n  worktrees: removed=1 failed=2\n"
        );
    }

    #[test]
    fn renders_remove_result_hook_warnings() {
        let output = render_remove_human(
            "s-42",
            &SessionRemoveResult {
                removed: true,
                stopped: false,
                worktrees_removed: 1,
                worktrees_failed: 0,
                accepted_unconfirmed_processes: Vec::new(),
                warnings: vec![protocol::SessionWarning {
                    kind: protocol::SessionWarningKind::Hook,
                    message: "pre-remove hook failed".to_owned(),
                    detail: Some("exit status 1\nnetwork busy".to_owned()),
                }],
            },
        );

        assert_eq!(
            output,
            concat!(
                "session s-42: removed=true stopped=false\n",
                "  worktrees: removed=1 failed=0\n",
                "  warning [hook]: pre-remove hook failed\n",
                "    detail: exit status 1\n",
                "      network busy\n"
            )
        );
    }

    /// A sweep result with the given failure counters and nothing else set.
    fn sweep_result(failed: u32, worktrees_failed: u32) -> SessionRetentionResult {
        SessionRetentionResult {
            dry_run: false,
            examined: 4,
            eligible: 2,
            held: 1,
            removed: 2,
            worktrees_cleaned: 1,
            worktrees_failed,
            failed,
            sessions: Vec::new(),
        }
    }

    #[test]
    fn a_sweep_succeeds_only_without_a_failed_removal_or_leftover_worktree() {
        assert!(sweep_succeeded(&sweep_result(0, 0)));
        assert!(
            !sweep_succeeded(&sweep_result(1, 0)),
            "a session it could not remove is a partial failure"
        );
        assert!(
            !sweep_succeeded(&sweep_result(0, 1)),
            "a worktree it could not delete is a partial failure"
        );
    }

    #[test]
    fn renders_sweep_result_with_holds_and_failure_counters() {
        let mut result = sweep_result(1, 2);
        result.sessions.push(protocol::SessionRetentionCandidate {
            session_id: protocol::SessionId("s-42".to_owned()),
            reason: protocol::SessionRetentionReason::Lost,
            age_secs: 90,
            worktree_path: Some(std::path::PathBuf::from("/data/worktrees/s-42")),
            hold: Some(protocol::SessionRetentionHold::WorktreeUncommitted),
            removed: false,
        });

        let output = render_sweep_human("local", &result);

        assert_eq!(
            output,
            concat!(
                "host local session retention removed 2 of 4 session(s)\n",
                "  eligible=2 held=1 removed=2 worktrees_cleaned=1 worktrees_failed=2 failed=1\n",
                "  s-42 reason=lost age_secs=90 hold=worktree_uncommitted removed=false",
                " worktree=/data/worktrees/s-42\n"
            )
        );
    }

    #[test]
    fn renders_fork_result_with_new_session_id() {
        let output = render_fork_human(&running_session("s-99"));

        assert_eq!(output, "session s-99 forked (state: running)\n");
    }

    #[tokio::test]
    async fn run_remove_calls_the_consenting_method_only_with_explicit_consent() {
        for (accept_unconfirmed_cleanup, expected_method) in [
            (true, method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED),
            (false, method::SESSION_REMOVE),
        ] {
            let mut daemon = crate::test_daemon::HeldDaemon::start(json!({
                "removed": true,
                "stopped": true,
                "worktrees_removed": 0,
                "worktrees_failed": 0
            }));
            let paths = daemon.paths.clone();
            let target: Target = "local/s-42".parse().expect("target");

            let remove = tokio::spawn(async move {
                run_remove(
                    &PinnedHost::local(),
                    &paths,
                    &target,
                    accept_unconfirmed_cleanup,
                    true,
                )
                .await
            });
            let received = pohunek_test_support::wait::guard(
                "the fixture to record the request",
                daemon.request(),
            )
            .await;
            daemon.release();
            pohunek_test_support::wait::guard("session remove against the fixture daemon", remove)
                .await
                .expect("remove task")
                .expect("remove succeeds against the fixture daemon");

            assert_eq!(
                received.request.method(),
                expected_method,
                "accept_unconfirmed_cleanup={accept_unconfirmed_cleanup}"
            );
            assert_eq!(received.request.params(), &json!("s-42"));
            assert!(
                !received.on_shared_connection,
                "a remove runs on a dedicated connection"
            );
        }
    }

    /// Margin past a lifecycle budget that covers the client's response
    /// headroom.
    const BUDGET_MARGIN: Duration = Duration::from_secs(2);

    #[tokio::test(start_paused = true)]
    async fn lifecycle_commands_report_an_unknown_outcome_past_their_budget() {
        use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};

        type Command = for<'a> fn(
            &'a Paths,
            &'a Target,
            bool,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), CliError>> + Send + 'a>,
        >;
        let commands: [(&str, u32, &str, Command); 3] = [
            (
                "stop",
                pohunek_client::SESSION_STOP_REQUEST_BUDGET_MS,
                "session_stop_outcome_unknown",
                |paths, target, json| {
                    Box::pin(
                        async move { run_stop(&PinnedHost::local(), paths, target, json).await },
                    )
                },
            ),
            (
                "fork",
                pohunek_client::SESSION_FORK_REQUEST_BUDGET_MS,
                "session_fork_outcome_unknown",
                |paths, target, json| {
                    Box::pin(async move {
                        run_fork(&PinnedHost::local(), paths, target, None, false, json).await
                    })
                },
            ),
            (
                "remove",
                pohunek_client::SESSION_REMOVE_REQUEST_BUDGET_MS,
                "session_remove_outcome_unknown",
                |paths, target, json| {
                    Box::pin(async move {
                        run_remove(&PinnedHost::local(), paths, target, false, json).await
                    })
                },
            ),
        ];
        let inhibitor = AutoAdvanceInhibitor::new();
        for (name, budget_ms, code, command) in commands {
            for json in [false, true] {
                let mut daemon = crate::test_daemon::HeldDaemon::start(json!({}));
                let paths = daemon.paths.clone();
                let target: Target = "local/s-42".parse().expect("target");
                let run = tokio::spawn(async move { command(&paths, &target, json).await });
                daemon.request().await;
                tokio::time::advance(
                    Duration::from_millis(u64::from(budget_ms)) + BUDGET_MARGIN + TIMER_TICK,
                )
                .await;

                let error = run
                    .await
                    .expect("command task")
                    .expect_err("the lifecycle budget elapses before the daemon answers");
                let structured = error.to_protocol_error();
                assert_eq!(structured.code, code, "{name} json={json}");
                assert_eq!(structured.class, protocol::ErrorClass::Transport);
                assert!(structured.recover.is_some(), "{name} json={json}");
                daemon.release();
            }
        }
        inhibitor.release().await;
    }

    #[test]
    fn a_daemon_without_the_consent_method_gets_an_upgrade_hint() {
        let missing = CliError::Protocol(protocol::ProtocolError::method_not_found(
            "session.remove_accepting_unconfirmed",
        ));
        match missing_consent_method_hint(missing) {
            CliError::Protocol(source) => {
                assert_eq!(source.code, "method_not_found");
                assert!(source.recover.expect("hint").contains("upgrade the daemon"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let remote = CliError::Client(pohunek_client::ClientError::RemoteProtocol {
            host: "peer-a".to_owned(),
            source: protocol::ProtocolError::method_not_found(
                "session.remove_accepting_unconfirmed",
            ),
        });
        match missing_consent_method_hint(remote) {
            CliError::Client(pohunek_client::ClientError::RemoteProtocol { host, source }) => {
                assert_eq!(host, "peer-a");
                assert_eq!(source.code, "method_not_found");
                assert!(source.recover.expect("hint").contains("upgrade the daemon"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let remote_other = CliError::Client(pohunek_client::ClientError::RemoteProtocol {
            host: "peer-a".to_owned(),
            source: protocol::ProtocolError::bad_request("x"),
        });
        match missing_consent_method_hint(remote_other) {
            CliError::Client(pohunek_client::ClientError::RemoteProtocol { source, .. }) => {
                assert!(source.recover.is_none());
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let other = CliError::Protocol(protocol::ProtocolError::bad_request("x"));
        match missing_consent_method_hint(other) {
            CliError::Protocol(source) => assert!(source.recover.is_none()),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn remove_json_output_passes_the_accepted_processes_through() {
        let result = SessionRemoveResult {
            removed: true,
            stopped: false,
            worktrees_removed: 0,
            worktrees_failed: 0,
            accepted_unconfirmed_processes: vec![protocol::UnconfirmedProcess {
                pid: 4242,
                start_identity: protocol::ProcessStartIdentity::new(99),
                command: Some("gpg-agent".to_owned()),
            }],
            warnings: Vec::new(),
        };

        let rendered = crate::commands::render_json(&result).expect("render json");
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");

        assert_eq!(
            value["ok"]["accepted_unconfirmed_processes"],
            serde_json::json!([{"pid": 4242, "start_identity": "99", "command": "gpg-agent"}])
        );
    }

    #[test]
    fn build_fork_request_carries_the_profile_override_only_when_asked() {
        let target: Target = "host-a/s-42".parse().expect("target");

        let request = build_fork_request(&target, Some("forked review".to_owned()), false, 100, 30)
            .expect("build fork request");
        assert_request(
            &request,
            method::SESSION_FORK,
            serde_json::json!({
                "session_id": "s-42",
                "name": "forked review",
                "cwd_mode": "same",
                "cols": 100,
                "rows": 30
            }),
        );

        let request = build_fork_request(&target, None, true, 100, 30).expect("build fork request");
        assert_request(
            &request,
            method::SESSION_FORK,
            serde_json::json!({
                "session_id": "s-42",
                "cwd_mode": "same",
                "cols": 100,
                "rows": 30,
                "accept_profile_change": true
            }),
        );
    }

    #[test]
    fn resume_params_keep_the_bare_id_shape_unless_the_override_is_asked() {
        let target: Target = "s-42".parse().expect("target");

        assert_eq!(
            serde_json::to_value(resume_params(&target, false)).expect("serialize"),
            serde_json::json!("s-42"),
            "an older daemon only understands the bare id"
        );
        assert_eq!(
            serde_json::to_value(resume_params(&target, true)).expect("serialize"),
            serde_json::json!({"session_id": "s-42", "accept_profile_change": true})
        );
    }

    #[test]
    fn renders_detection_previews_with_empty_regions_visible() {
        let result = SessionDetectionResult {
            session_id: SessionId("s-42".to_owned()),
            supported_regions: protocol::DetectionRegionKind::ALL.to_vec(),
            previews: vec![
                protocol::DetectionRegionPreview {
                    kind: protocol::DetectionRegionKind::TopNonEmptyLines,
                    region: "top_non_empty_lines(8)".to_owned(),
                    text: "trust this repository".to_owned(),
                },
                protocol::DetectionRegionPreview {
                    kind: protocol::DetectionRegionKind::LastNonEmptyAbovePromptBox,
                    region: "last_non_empty_above_prompt_box".to_owned(),
                    text: String::new(),
                },
            ],
        };

        assert_eq!(
            render_detection_human(&result),
            "--- top_non_empty_lines(8) ---\ntrust this repository\n--- last_non_empty_above_prompt_box ---\n<empty>\n"
        );
    }

    #[test]
    fn bounded_input_accepts_multiline_utf8() {
        let input = "first line\nsecond řádek\tvalue\r\n";
        assert_eq!(
            read_bounded_input(input.as_bytes()).expect("valid stdin"),
            input
        );
    }

    #[test]
    fn bounded_input_rejects_limit_plus_one_without_payload() {
        let input = vec![b'x'; MAX_STDIN_INPUT_BYTES + 1];
        let error = read_bounded_input(input.as_slice()).expect_err("oversized stdin");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains(&MAX_STDIN_INPUT_BYTES.to_string()));
        assert!(!diagnostic.contains("xxxx"));
    }

    #[test]
    fn output_limit_parser_enforces_protocol_boundary() {
        assert_eq!(
            parse_output_bytes(&MAX_SESSION_OUTPUT_BYTES.to_string()).expect("maximum"),
            u32::try_from(MAX_SESSION_OUTPUT_BYTES).expect("limit fits u32")
        );
        parse_output_bytes("0").expect_err("zero is invalid");
        parse_output_bytes(&(MAX_SESSION_OUTPUT_BYTES + 1).to_string())
            .expect_err("limit plus one is invalid");
    }

    #[test]
    fn exact_name_resolution_prefers_id_and_rejects_sorted_ambiguity() {
        let mut first = running_session("s-2");
        first.name = Some("review".to_owned());
        let mut second = running_session("s-1");
        second.name = Some("review".to_owned());
        let sessions = vec![first, second];

        let id: Target = "s-2".parse().expect("target");
        assert_eq!(
            resolve_target_from_sessions(&id, &sessions)
                .expect("exact id")
                .session_id,
            "s-2"
        );

        let name: Target = "review".parse().expect("target");
        let error = resolve_target_from_sessions(&name, &sessions).expect_err("ambiguous");
        assert_eq!(error.to_protocol_error().code, "ambiguous_session_name");
        assert!(error.to_string().contains("s-1, s-2"));
    }

    #[test]
    fn exact_unique_name_resolution_preserves_explicit_host() {
        let mut session = running_session("s-42");
        session.name = Some("review".to_owned());
        let target: Target = "host-b/review".parse().expect("target");
        let resolved = resolve_target_from_sessions(&target, &[session]).expect("unique name");
        assert_eq!(resolved.host.as_deref(), Some("host-b"));
        assert_eq!(resolved.session_id, "s-42");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installed_cancellation_queues_a_signal_raised_before_it_is_polled() {
        let mut cancellation = ProcessCancellation::install().expect("install signal handlers");
        #[expect(
            unsafe_code,
            reason = "libc::raise delivers the signal whose handler the test just installed"
        )]
        // SAFETY: `raise` only delivers SIGTERM to this process, and the handler
        // for it was registered by `install` above.
        let result = unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(result, 0, "raise SIGTERM");
        pohunek_test_support::wait::guard("the queued SIGTERM", cancellation.cancelled()).await;
    }
}
