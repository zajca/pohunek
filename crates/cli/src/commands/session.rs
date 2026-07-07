//! `pohunek session` — manage PTY-backed sessions on a local or remote host.
//!
//! The CLI grammar is host-aware through [`crate::target::Target`]; the effective
//! host selects the transport ([`Client`] dials the local Unix socket or a remote
//! `NetBird` TCP connection). The session-id is the same on either side, so the
//! request-building functions are transport-agnostic. Starting a session on a
//! *remote* host goes through a confirmation gate (see [`confirmation_decision`]).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

#[cfg(test)]
use protocol::Request;
use protocol::{
    method, AgentActivity, CwdSource, ForkCwdMode, SessionForkParams, SessionId, SessionInfo,
    SessionInputParams, SessionInputResult, SessionListFilter, SessionListParams, SessionNewParams,
    SessionRemoveResult, SessionRenameParams, SessionState, SessionStopResult, SessionWarningKind,
    StateSource,
};

use crate::client::Client;
#[cfg(test)]
use crate::commands::request_with_params;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{is_local_host, Target};

/// Default fork PTY width when the command is not launched from an attach view.
///
/// Matches `session new`'s CLI default so non-interactive fork callers get the
/// same baseline terminal geometry unless a richer surface supplies live size.
const DEFAULT_FORK_COLS: u16 = 80;
/// Default fork PTY height when the command is not launched from an attach view.
///
/// Matches `session new`'s CLI default; attach and GUI surfaces send live size.
const DEFAULT_FORK_ROWS: u16 = 24;

/// Arguments for `session new`, grouped to keep the call site readable as the
/// optional worktree flags accumulate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewArgs {
    /// Agent NAME to start: a base kind (`shell`/`codex`/`claude`) or a host
    /// profile, resolved daemon-side on the target host (Part C, free string).
    pub agent: String,
    /// Owner-set display name for the session, or `None` to show it by id.
    pub name: Option<String>,
    /// Working directory (ignored when a worktree is bound).
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
    /// Initial text to inject into the spawned PTY.
    pub input: Option<String>,
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
        }
    }
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

fn parse_state_filter(value: &str) -> Result<SessionState, String> {
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

fn parse_activity_filter(value: &str) -> Result<AgentActivity, String> {
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

    let mut client = Client::connect(host, paths).await?;
    let result = client.call::<method::SessionNew>(new_params(&args)).await?;
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

    if json {
        print!("{}", crate::commands::render_json(info)?);
    } else {
        print!("{}", render_new_human(info));
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
    host: &str,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let info = client
        .call::<method::SessionInspect>(SessionId(target.session_id.clone()))
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&info)?);
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
    host: &str,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let stop = client
        .call::<method::SessionStop>(SessionId(target.session_id.clone()))
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
    host: &str,
    paths: &Paths,
    target: &Target,
    name: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let forked = client
        .call::<method::SessionFork>(fork_params(
            target,
            name,
            DEFAULT_FORK_COLS,
            DEFAULT_FORK_ROWS,
        ))
        .await?;
    let info = &forked.session;

    if json {
        print!("{}", crate::commands::render_json(info)?);
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
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, the daemon rejects the request, or the payload does not match the
/// contract.
pub(crate) async fn run_remove(
    host: &str,
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let removed = client
        .call::<method::SessionRemove>(SessionId(target.session_id.clone()))
        .await?;

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
    host: &str,
    paths: &Paths,
    target: &Target,
    text: &str,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let input = client
        .call::<method::SessionInput>(input_params(target, text))
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&input)?);
    } else {
        print!("{}", render_input_human(&target.session_id, &input));
    }
    Ok(())
}

/// Set (`Some`) or clear (`None`) a session's display name on the target host.
pub(crate) async fn run_rename(
    host: &str,
    paths: &Paths,
    target: &Target,
    name: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let renamed = client
        .call::<method::SessionRename>(rename_params(target, name))
        .await?;

    if json {
        print!("{}", crate::commands::render_json(&renamed)?);
    } else {
        print!("{}", render_rename_human(&renamed.session));
    }
    Ok(())
}

fn new_params(args: &NewArgs) -> SessionNewParams {
    SessionNewParams {
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
        metadata: std::collections::BTreeMap::new(),
    }
}

#[cfg(test)]
fn build_new_request(args: &NewArgs) -> Result<Request, CliError> {
    request_with_params(method::SESSION_NEW, &new_params(args))
}

/// Prepare the `session new` args for the target host (design Decision 1).
///
/// Local: send the CLI's **own** `current_dir()` as `cwd` (unless the user pinned
/// `--cwd`) so the daemon auto-detects the project we are standing in. Remote: send
/// **no** local path — a filesystem path is meaningless on another host — and
/// require a `--project` reference (or `--repo` for first-introduction), failing
/// fast before any connection is dialed.
fn prepare_new_args(host: &str, args: NewArgs) -> Result<NewArgs, CliError> {
    let remote = !is_local_host(host);
    if remote {
        if args.project.is_none() && args.repo.is_none() {
            return Err(CliError::RemoteTargetRequired);
        }
        // Drop any local --cwd: it cannot be resolved on the remote host.
        Ok(NewArgs { cwd: None, ..args })
    } else {
        let cwd = match args.cwd {
            Some(cwd) => Some(cwd),
            None => Some(std::env::current_dir()?),
        };
        Ok(NewArgs { cwd, ..args })
    }
}

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

#[cfg(test)]
fn build_inspect_request(target: &Target) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_INSPECT,
        &SessionId(target.session_id.clone()),
    )
}

#[cfg(test)]
fn build_stop_request(target: &Target) -> Result<Request, CliError> {
    request_with_params(method::SESSION_STOP, &SessionId(target.session_id.clone()))
}

#[cfg(test)]
fn build_remove_request(target: &Target) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_REMOVE,
        &SessionId(target.session_id.clone()),
    )
}

fn fork_params(target: &Target, name: Option<String>, cols: u16, rows: u16) -> SessionForkParams {
    SessionForkParams {
        session_id: SessionId(target.session_id.clone()),
        name,
        cwd_mode: ForkCwdMode::Same,
        cols,
        rows,
    }
}

#[cfg(test)]
fn build_fork_request(
    target: &Target,
    name: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<Request, CliError> {
    request_with_params(method::SESSION_FORK, &fork_params(target, name, cols, rows))
}

fn input_params(target: &Target, text: &str) -> SessionInputParams {
    SessionInputParams {
        session_id: SessionId(target.session_id.clone()),
        text: text.to_owned(),
    }
}

#[cfg(test)]
fn build_input_request(target: &Target, text: &str) -> Result<Request, CliError> {
    request_with_params(method::SESSION_INPUT, &input_params(target, text))
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

fn render_new_human(info: &SessionInfo) -> String {
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
    for warning in &info.warnings {
        let _ = writeln!(
            output,
            "  warning [{}]: {}",
            warning_kind_label(warning.kind),
            warning.message
        );
    }
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

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{:<id_width$}  {:<name_width$}  {:<agent_width$}  {:<8}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {:<project_width$}  {:<branch_width$}  {:<4}  CWD",
        "ID", "NAME", "AGENT", "ORIGIN", "STATE", "ACTIVITY", "SOURCE", "PID", "SIZE", "PROJECT", "BRANCH", "WARN",
    );
    for session in sessions {
        let _ = writeln!(
            output,
            "{:<id_width$}  {:<name_width$}  {:<agent_width$}  {:<8}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {:<project_width$}  {:<branch_width$}  {:<4}  {}",
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
            session.cwd.display(),
        );
    }
    output
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
    // (`build_list_request` sends them as typed params), but a daemon that
    // predates the filter API ignores unknown params and returns every session.
    // Re-applying the predicate here guarantees a correctly filtered set against
    // such a peer. `ListFilter::matches` mirrors `protocol::SessionListFilter::matches`
    // exactly (a unit test pins that they agree), so this never narrows further
    // than the daemon would.
    let filtered: Vec<SessionInfo> = sessions
        .iter()
        .filter(|session| session_matches_filters(session, filters))
        .cloned()
        .collect();

    match output_mode {
        ListOutputMode::Human => Ok(render_list_human(&filtered)),
        ListOutputMode::Json => crate::commands::render_json(&filtered),
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
            "resumable",
            // Resumable only while the session can still come back: it needs a
            // captured native id AND a non-terminal state. On exit the daemon
            // drops the resume binding, so a terminal session whose native id
            // still lingers in its info is no longer resumable.
            if info.native_session_id.is_some() && !is_terminal(info.state) {
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
        ("created_at", info.created_at.clone()),
        ("updated_at", info.updated_at.clone()),
        (
            "exit_code",
            info.exit_code.map_or_else(none, |code| code.to_string()),
        ),
    ];
    if let Some(active_agent) = &info.active_agent {
        rows.insert(3, ("active_agent", active_agent.clone()));
    }
    if let Some(active_base) = info.active_agent_base {
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
    // Each non-fatal warning is detailed below the table so a worktree session
    // surfaces exactly what happened and what was done instead.
    for warning in &info.warnings {
        let _ = writeln!(
            output,
            "warning [{}]: {}",
            warning_kind_label(warning.kind),
            warning.message
        );
        if let Some(detail) = &warning.detail {
            let _ = writeln!(output, "  detail: {detail}");
        }
    }
    output
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

fn render_remove_human(session_id: &str, result: &SessionRemoveResult) -> String {
    format!(
        "session {session_id}: removed={} stopped={}\n",
        result.removed, result.stopped
    )
}

fn render_input_human(session_id: &str, result: &SessionInputResult) -> String {
    if result.accepted {
        format!("session {session_id}: input accepted\n")
    } else {
        format!("session {session_id}: input rejected\n")
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

fn agent_kind_label(agent: protocol::AgentKind) -> &'static str {
    match agent {
        protocol::AgentKind::Shell => "shell",
        protocol::AgentKind::Codex => "codex",
        protocol::AgentKind::Claude => "claude",
    }
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

/// Whether a session has reached a terminal state (no further transitions).
/// Mirrors the daemon's terminal-state set: a terminal session has had its
/// resume binding dropped and can no longer be resumed.
fn is_terminal(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Stopped | SessionState::Done | SessionState::Failed
    )
}

fn activity_label_option(activity: Option<AgentActivity>) -> &'static str {
    activity.map_or("-", activity_label)
}

fn activity_label(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "blocked",
        AgentActivity::Idle => "idle",
    }
}

fn state_source_label(source: StateSource) -> &'static str {
    match source {
        StateSource::OscTitle => "osc_title",
        StateSource::OscProgress => "osc_progress",
        StateSource::Screen => "screen",
        StateSource::Process => "process",
        StateSource::Report => "report",
    }
}

fn warning_kind_label(kind: SessionWarningKind) -> &'static str {
    match kind {
        SessionWarningKind::Fetch => "fetch",
        SessionWarningKind::BaseBranchFallback => "base_branch_fallback",
        SessionWarningKind::SetupScript => "setup_script",
        SessionWarningKind::Hook => "hook",
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
            agent: "shell".to_owned(),
            agent_base: protocol::AgentKind::Shell,
            cwd: PathBuf::from("/workspace/project"),
            cwd_source: Some(protocol::CwdSource::Launch),
            pid: 4242,
            cols: 120,
            rows: 40,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            native_session_id: None,
            native_session_path: None,
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
            created_at: "2026-06-17T10:00:00Z".to_owned(),
            updated_at: "2026-06-17T10:01:00Z".to_owned(),
            exit_code: None,
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
            input: None,
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "test helper takes the json! literal by value to keep call sites terse"
    )]
    fn assert_request(request: &Request, method_name: &str, params: serde_json::Value) {
        assert_eq!(request.v.get(), 1, "envelope version");
        assert_eq!(request.method, method_name, "method");
        assert_eq!(request.params, params, "params");
        // The id is now a unique per-call SDK correlation id, not a fixed string;
        // assert only its stable, log-greppable `sdk-<method>-` prefix.
        assert!(
            request.id.starts_with(&format!("sdk-{method_name}-")),
            "id {:?} must be prefixed by the method",
            request.id
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
    fn new_request_accepts_codex_agent() {
        let request = build_new_request(&new_args("codex", None)).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "codex",
                "cols": 80,
                "rows": 24
            }),
        );
    }

    #[test]
    fn new_request_accepts_claude_agent() {
        let request = build_new_request(&new_args("claude", None)).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "claude",
                "cols": 80,
                "rows": 24
            }),
        );
    }

    #[test]
    fn new_request_carries_worktree_repo_branch_and_base() {
        let args = NewArgs {
            agent: "claude".to_owned(),
            name: None,
            cwd: None,
            cols: 80,
            rows: 24,
            project: None,
            repo: Some(PathBuf::from("/workspace/project")),
            branch: Some("feature/login".to_owned()),
            base_branch: Some("main".to_owned()),
            input: None,
        };
        let request = build_new_request(&args).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "claude",
                "cols": 80,
                "rows": 24,
                "repo": "/workspace/project",
                "branch": "feature/login",
                "base_branch": "main"
            }),
        );
    }

    #[test]
    fn new_request_includes_cwd_and_requested_size() {
        let request = build_new_request(&NewArgs {
            agent: "shell".to_owned(),
            name: None,
            cwd: Some(PathBuf::from("/workspace/project")),
            cols: 120,
            rows: 40,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
        })
        .expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "shell",
                "cwd": "/workspace/project",
                "cols": 120,
                "rows": 40
            }),
        );
    }

    #[test]
    fn new_request_carries_initial_input() {
        let mut args = new_args("shell", None);
        args.input = Some("Fix #1234".to_owned());
        let request = build_new_request(&args).expect("request");

        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "shell",
                "cols": 80,
                "rows": 24,
                "input": "Fix #1234"
            }),
        );
    }

    #[test]
    fn list_request_uses_empty_typed_params() {
        let request = build_list_request(&[]).expect("request");

        assert_request(&request, method::SESSION_LIST, json!({}));
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
    fn list_filter_matches_single_field() {
        let filter = parse_list_filter("state=running").expect("filter");

        assert!(filter.matches(&running_session("s-42")));
    }

    #[test]
    fn list_filter_rejects_non_matching_field() {
        let filter = parse_list_filter("agent=codex").expect("filter");

        assert!(!filter.matches(&running_session("s-42")));
    }

    #[test]
    fn list_filter_matches_active_agent_identity() {
        let mut session = running_session("s-42");
        session.active_agent = Some("codex".to_owned());
        session.active_agent_base = Some(protocol::AgentKind::Codex);

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
    fn list_params_preserve_filters_for_typed_sdk_call() {
        let params = list_params(&[
            ListFilter::State(SessionState::Running),
            ListFilter::Project("ui".to_owned()),
        ]);

        assert_eq!(
            params.filters,
            vec![
                SessionListFilter::State(SessionState::Running),
                SessionListFilter::Project("ui".to_owned()),
            ]
        );
    }

    #[test]
    fn list_params_without_filters_is_empty_for_typed_sdk_call() {
        let params = list_params(&[]);

        assert!(params.filters.is_empty());
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
    fn list_filter_unknown_key_is_error() {
        let err = parse_list_filter("cwd=/workspace/project").expect_err("unknown key");

        assert!(err.contains("unknown filter key"), "{err}");
    }

    #[test]
    fn list_filter_bad_value_is_error() {
        let err = parse_list_filter("state=paused").expect_err("bad value");

        assert!(err.contains("invalid state filter value"), "{err}");
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
        let json_ids: Vec<String> = serde_json::from_str::<Vec<SessionInfo>>(&json_output)
            .expect("json sessions")
            .into_iter()
            .map(|session| session.id.0)
            .collect();
        let quiet_ids: Vec<&str> = quiet_output.lines().collect();

        assert_eq!(quiet_ids, json_ids);
        assert_eq!(json_ids, vec!["s-shell", "s-codex"]);
    }

    #[test]
    fn input_request_sends_local_session_id_and_text() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_input_request(&target, "write tests first").expect("request");

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
    fn input_request_extracts_session_id_regardless_of_host() {
        // Remote is now supported: the request carries only the session id (host
        // routing is the transport's job), and it is extracted identically from a
        // remote target as from a local one.
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
    fn rename_request_sends_session_id_and_name() {
        let target: Target = "host-b/s-42".parse().expect("target");
        let request =
            build_rename_request(&target, Some("triage build".to_owned())).expect("request");

        assert_request(
            &request,
            method::SESSION_RENAME,
            json!({"session_id": "s-42", "name": "triage build"}),
        );
    }

    #[test]
    fn rename_request_omits_name_when_clearing() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_rename_request(&target, None).expect("request");

        // A cleared name is omitted from the wire (the daemon treats absence as
        // clear), so the body carries only the session id.
        assert_request(
            &request,
            method::SESSION_RENAME,
            json!({"session_id": "s-42"}),
        );
    }

    #[test]
    fn renders_session_name_in_list_and_inspect() {
        let mut session = running_session("s-42");
        session.name = Some("triage".to_owned());

        let list = render_list_human(&[session.clone()]);
        assert_eq!(
            list_row(&list, "s-42")[1],
            "triage",
            "NAME column shows name"
        );

        let inspect = render_inspect_human(&session);
        assert!(
            has_row(&inspect, "name", "triage"),
            "inspect shows the name row: {inspect}"
        );
    }

    #[test]
    fn inspect_request_sends_only_session_id() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_inspect_request(&target).expect("request");

        assert_request(&request, method::SESSION_INSPECT, json!("s-42"));
    }

    #[test]
    fn stop_request_extracts_session_id_regardless_of_host() {
        // A remote target's session id is sent unchanged; the host never leaks
        // into the request body.
        let remote: Target = "host-b/s-42".parse().expect("target");
        let request = build_stop_request(&remote).expect("request");

        assert_request(&request, method::SESSION_STOP, json!("s-42"));
    }

    #[test]
    fn prepare_new_args_local_defaults_cwd_to_current_dir() {
        // A local session sends the CLI's own cwd so the daemon auto-detects the
        // project we are standing in.
        let prepared =
            prepare_new_args(LOCAL_HOST, new_args("shell", None)).expect("local prepare succeeds");
        assert_eq!(
            prepared.cwd,
            Some(std::env::current_dir().expect("cwd")),
            "local defaults cwd to the CLI's own current dir"
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
        // No filesystem path crosses the wire to a remote host: a remote start must
        // name a --project (or --repo), and is rejected before any dial.
        let err = prepare_new_args("host-b", new_args("shell", Some(PathBuf::from("/x"))))
            .expect_err("remote without a target is rejected");
        assert!(
            matches!(err, CliError::RemoteTargetRequired),
            "expected RemoteTargetRequired, got {err:?}"
        );
    }

    #[test]
    fn prepare_new_args_remote_with_project_drops_local_cwd() {
        let mut args = new_args("shell", Some(PathBuf::from("/local/path")));
        args.project = Some("ui".to_owned());
        let prepared = prepare_new_args("host-b", args).expect("remote with --project");
        assert_eq!(
            prepared.cwd, None,
            "a local cwd must not be sent to a remote"
        );
        assert_eq!(prepared.project.as_deref(), Some("ui"));
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
    fn new_request_carries_project_reference() {
        let mut args = new_args("claude", None);
        args.project = Some("ui".to_owned());
        let request = build_new_request(&args).expect("request");
        assert_request(
            &request,
            method::SESSION_NEW,
            json!({
                "agent": "claude",
                "cols": 80,
                "rows": 24,
                "project": "ui"
            }),
        );
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

    #[test]
    fn renders_new_session_summary() {
        let output = render_new_human(&running_session("s-42"));

        assert_eq!(output, "session s-42 created (state: running)\n");
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
            "ID", "NAME", "AGENT", "ORIGIN", "STATE", "PROJECT", "BRANCH", "WARN", "CWD",
        ] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        // Columns: ID NAME AGENT ORIGIN STATE ACTIVITY SOURCE PID SIZE PROJECT BRANCH WARN CWD.
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
                "/workspace/project",
            ]
        );
    }

    #[test]
    fn renders_detected_activity_in_session_list_table() {
        let mut session = running_session("s-42");
        session.activity = Some(AgentActivity::Working);
        session.state_source = StateSource::OscTitle;

        let output = render_list_human(&[session]);

        assert_eq!(
            list_row(&output, "s-42"),
            vec![
                "s-42",
                "-", // NAME
                "shell",
                "managed",
                "running",
                "working",
                "osc_title",
                "4242",
                "120x40",
                "-", // PROJECT
                "-", // BRANCH
                "-", // WARN
                "/workspace/project",
            ]
        );
    }

    #[test]
    fn renders_external_origin_in_session_list_table() {
        let mut session = running_session("ext-42");
        session.external = Some(true);

        let output = render_list_human(&[session]);

        assert_eq!(list_row(&output, "ext-42")[3], "external");
    }

    #[test]
    fn renders_codex_and_claude_agents_in_session_list_table() {
        let mut codex = running_session("s-codex");
        codex.agent = "codex".to_owned();
        let mut claude = running_session("s-claude");
        claude.agent = "claude".to_owned();

        let output = render_list_human(&[codex, claude]);

        assert_eq!(list_row(&output, "s-codex")[2], "codex");
        assert_eq!(list_row(&output, "s-claude")[2], "claude");
    }

    #[test]
    fn renders_active_agent_in_session_list_table() {
        let mut session = running_session("s-42");
        session.active_agent = Some("codex".to_owned());
        session.active_agent_base = Some(protocol::AgentKind::Codex);

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
        // Columns: ID NAME AGENT ORIGIN STATE ACTIVITY SOURCE PID SIZE PROJECT BRANCH WARN CWD.
        assert_eq!(
            row[9], "login-ui",
            "project column shows the label: {row:?}"
        );
        assert_eq!(row[10], "feature/login", "branch column: {row:?}");
        assert_eq!(row[11], "1", "warning-count column: {row:?}");
        assert_eq!(
            row[12], "/data/worktrees/s-42-project-feature-login",
            "cwd is the worktree path: {row:?}"
        );
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
        assert!(has_row(&output, "resumable", "no"));
        assert!(has_row(&output, "exit_code", "<none>"));
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
    fn renders_native_session_id_and_resumable_when_captured() {
        let mut session = running_session("s-42");
        session.native_session_id = Some("native-abc".to_owned());

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "native_session_id", "native-abc"));
        assert!(has_row(&output, "resumable", "yes"));
    }

    #[test]
    fn terminal_session_is_not_resumable_despite_captured_native_id() {
        for state in [
            SessionState::Stopped,
            SessionState::Done,
            SessionState::Failed,
        ] {
            let mut session = running_session("s-42");
            session.native_session_id = Some("native-abc".to_owned());
            session.state = state;

            let output = render_inspect_human(&session);

            // The native id stays visible for reference (e.g. a manual resume
            // outside the tool), but the daemon drops the resume binding on
            // exit, so a terminal session must not report resumable=yes.
            assert!(
                has_row(&output, "native_session_id", "native-abc"),
                "native id stays visible in state {state:?}: {output}"
            );
            assert!(
                has_row(&output, "resumable", "no"),
                "a terminal session ({state:?}) must not report resumable=yes: {output}"
            );
        }
    }

    #[test]
    fn renders_claude_agent_in_session_inspect_table() {
        let mut session = running_session("s-42");
        session.agent = "claude".to_owned();

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "agent", "claude"));
    }

    #[test]
    fn renders_active_agent_fields_in_session_inspect_table() {
        let mut session = running_session("s-42");
        session.active_agent = Some("codex".to_owned());
        session.active_agent_base = Some(protocol::AgentKind::Codex);
        session.active_agent_pid = Some(9001);
        session.active_agent_session_id = Some("codex-native".to_owned());
        session.active_agent_session_path = Some("/tmp/codex/session.json".to_owned());

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "agent", "shell"));
        assert!(has_row(&output, "active_agent", "codex"));
        assert!(has_row(&output, "active_base", "codex"));
        assert!(has_row(&output, "active_pid", "9001"));
        assert!(has_row(&output, "active_native_session_id", "codex-native"));
        assert!(has_row(
            &output,
            "active_native_session_path",
            "/tmp/codex/session.json"
        ));
        assert!(has_row(&output, "native_session_id", "<none>"));
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
    fn renders_inspect_worktree_fields_absent_as_none() {
        let output = render_inspect_human(&running_session("s-42"));
        assert!(has_row(&output, "repo", "<none>"));
        assert!(has_row(&output, "branch", "<none>"));
        assert!(has_row(&output, "worktree_path", "<none>"));
        assert!(has_row(&output, "warnings", "-"));
    }

    #[test]
    fn renders_new_summary_with_worktree_and_warnings() {
        let mut session = running_session("s-42");
        session.branch = Some("feature/login".to_owned());
        session.worktree_path = Some(PathBuf::from("/data/worktrees/s-42-project-feature-login"));
        session.cwd = session.worktree_path.clone().expect("worktree path");
        session.warnings = vec![SessionWarning {
            kind: SessionWarningKind::SetupScript,
            message: "Repository setup script failed; the worktree was kept without it.".to_owned(),
            detail: None,
        }];

        let output = render_new_human(&session);

        assert!(output.contains("session s-42 created"));
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
    }

    #[test]
    fn renders_input_result_with_target_id() {
        let output = render_input_human("s-42", &protocol::SessionInputResult { accepted: true });

        assert_eq!(output, "session s-42: input accepted\n");
    }

    #[test]
    fn renders_stop_result_with_target_id() {
        let output = render_stop_human("s-42", &protocol::SessionStopResult { stopped: true });

        assert_eq!(output, "session s-42: stopped=true\n");
    }

    #[test]
    fn renders_remove_result_with_target_id() {
        let output = render_remove_human(
            "s-42",
            &SessionRemoveResult {
                removed: true,
                stopped: true,
            },
        );

        assert_eq!(output, "session s-42: removed=true stopped=true\n");
    }

    #[test]
    fn renders_fork_result_with_new_session_id() {
        let output = render_fork_human(&running_session("s-99"));

        assert_eq!(output, "session s-99 forked (state: running)\n");
    }

    #[test]
    fn build_remove_request_targets_session_remove_method() {
        let target: Target = "local/s-42".parse().expect("parse target");
        let request = build_remove_request(&target).expect("build remove request");

        assert_request(&request, method::SESSION_REMOVE, serde_json::json!("s-42"));
    }

    #[test]
    fn build_fork_request_targets_session_fork_method() {
        let target: Target = "host-a/s-42".parse().expect("target");

        let request = build_fork_request(&target, Some("forked review".to_owned()), 100, 30)
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
    }

    #[test]
    fn renders_new_session_as_json_that_deserializes() {
        let info = running_session("s-42");
        let doc = crate::commands::render_json(&info).expect("json doc");
        let parsed: SessionInfo = serde_json::from_str(&doc).expect("parse session info");
        assert_eq!(parsed, info);
    }

    #[test]
    fn renders_session_list_as_json_that_deserializes() {
        let sessions = vec![running_session("s-1"), running_session("s-2")];
        let doc = crate::commands::render_json(&sessions).expect("json doc");
        let parsed: Vec<SessionInfo> = serde_json::from_str(&doc).expect("parse list");
        assert_eq!(parsed, sessions);
    }

    #[test]
    fn renders_inspect_as_json_that_deserializes() {
        let info = running_session("s-42");
        let doc = crate::commands::render_json(&info).expect("json doc");
        let parsed: SessionInfo = serde_json::from_str(&doc).expect("parse inspect");
        assert_eq!(parsed, info);
    }

    #[test]
    fn renders_stop_result_as_json_that_deserializes() {
        let result = protocol::SessionStopResult { stopped: true };
        let doc = crate::commands::render_json(&result).expect("json doc");
        let parsed: protocol::SessionStopResult =
            serde_json::from_str(&doc).expect("parse stop result");
        assert_eq!(parsed, result);
    }

    #[test]
    fn renders_input_result_as_json_that_deserializes() {
        let result = protocol::SessionInputResult { accepted: true };
        let doc = crate::commands::render_json(&result).expect("json doc");
        let parsed: protocol::SessionInputResult =
            serde_json::from_str(&doc).expect("parse input result");
        assert_eq!(parsed, result);
    }
}
