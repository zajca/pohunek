//! `zagentmesh session` — manage local PTY-backed sessions.
//!
//! Phase 1 only supports the local transport. The CLI
//! grammar is host-aware through [`crate::target::Target`], but remote targets
//! are rejected before any daemon request is sent.

use std::path::PathBuf;

use clap::ValueEnum;
use protocol::{
    method, AgentActivity, AgentKind, Request, SessionId, SessionInfo, SessionInputParams,
    SessionInputResult, SessionNewParams, SessionState, SessionStopResult, SessionWarningKind,
    StateSource,
};
use serde::Serialize;
use serde_json::Value;

use crate::client::LocalClient;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::Target;

/// Agent selector accepted by `session new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentArg {
    /// Start a plain shell session.
    Shell,
    /// Start a Codex CLI agent session.
    Codex,
    /// Start a Claude Code agent session.
    Claude,
}

impl From<AgentArg> for AgentKind {
    fn from(value: AgentArg) -> Self {
        match value {
            AgentArg::Shell => AgentKind::Shell,
            AgentArg::Codex => AgentKind::Codex,
            AgentArg::Claude => AgentKind::Claude,
        }
    }
}

/// Arguments for `session new`, grouped to keep the call site readable as the
/// optional worktree flags accumulate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewArgs {
    /// Agent kind to start.
    pub agent: AgentArg,
    /// Working directory (ignored when a worktree is bound).
    pub cwd: Option<PathBuf>,
    /// Initial terminal columns.
    pub cols: u16,
    /// Initial terminal rows.
    pub rows: u16,
    /// Repository to bind a worktree for.
    pub repo: Option<PathBuf>,
    /// Branch to check out in the worktree.
    pub branch: Option<String>,
    /// Base branch the worktree's branch is created from.
    pub base_branch: Option<String>,
}

/// Run `session new`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request, or
/// returns a payload that does not match the session contract.
pub(crate) async fn run_new(paths: &Paths, args: NewArgs) -> Result<(), CliError> {
    let mut client = LocalClient::connect(&paths.socket).await?;
    let request = build_new_request(&args)?;
    let result = client.request(&request).await?;
    let info: SessionInfo = serde_json::from_value(result)?;

    print!("{}", render_new_human(&info));
    Ok(())
}

/// Run `session list`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request, or
/// returns a payload that does not match the session contract.
pub(crate) async fn run_list(paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(&paths.socket).await?;
    let request = build_list_request();
    let result = client.request(&request).await?;
    let sessions: Vec<SessionInfo> = serde_json::from_value(result)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else {
        print!("{}", render_list_human(&sessions));
    }

    Ok(())
}

/// Run `session inspect`.
///
/// # Errors
///
/// Returns [`CliError`] if the target is remote, the daemon is unreachable,
/// rejects the request, or returns a payload that does not match the contract.
pub(crate) async fn run_inspect(
    paths: &Paths,
    target: &Target,
    json: bool,
) -> Result<(), CliError> {
    let request = build_inspect_request(target)?;
    let mut client = LocalClient::connect(&paths.socket).await?;
    let result = client.request(&request).await?;
    let info: SessionInfo = serde_json::from_value(result)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        print!("{}", render_inspect_human(&info));
    }

    Ok(())
}

/// Run `session stop`.
///
/// # Errors
///
/// Returns [`CliError`] if the target is remote, the daemon is unreachable,
/// rejects the request, or returns a payload that does not match the contract.
pub(crate) async fn run_stop(paths: &Paths, target: &Target) -> Result<(), CliError> {
    let request = build_stop_request(target)?;
    let mut client = LocalClient::connect(&paths.socket).await?;
    let result = client.request(&request).await?;
    let stop: SessionStopResult = serde_json::from_value(result)?;

    print!("{}", render_stop_human(&target.session_id, &stop));
    Ok(())
}

/// Run `session input`.
///
/// # Errors
///
/// Returns [`CliError`] if the target is remote, the daemon is unreachable,
/// rejects the request, or returns a payload that does not match the contract.
pub(crate) async fn run_input(paths: &Paths, target: &Target, text: &str) -> Result<(), CliError> {
    let request = build_input_request(target, text)?;
    let mut client = LocalClient::connect(&paths.socket).await?;
    let result = client.request(&request).await?;
    let input: SessionInputResult = serde_json::from_value(result)?;

    print!("{}", render_input_human(&target.session_id, &input));
    Ok(())
}

fn request_id(method: &str) -> String {
    format!("cli-{method}")
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

fn build_new_request(args: &NewArgs) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_NEW,
        &SessionNewParams {
            agent: args.agent.into(),
            cwd: args.cwd.clone(),
            cols: args.cols,
            rows: args.rows,
            repo: args.repo.clone(),
            branch: args.branch.clone(),
            base_branch: args.base_branch.clone(),
        },
    )
}

fn build_list_request() -> Request {
    Request::new(
        request_id(method::SESSION_LIST),
        method::SESSION_LIST,
        Value::Null,
    )
}

fn build_inspect_request(target: &Target) -> Result<Request, CliError> {
    let session_id = local_session_id(target)?;
    request_with_params(method::SESSION_INSPECT, &SessionId(session_id.to_owned()))
}

fn build_stop_request(target: &Target) -> Result<Request, CliError> {
    let session_id = local_session_id(target)?;
    request_with_params(method::SESSION_STOP, &SessionId(session_id.to_owned()))
}

fn build_input_request(target: &Target, text: &str) -> Result<Request, CliError> {
    let session_id = local_session_id(target)?;
    request_with_params(
        method::SESSION_INPUT,
        &SessionInputParams {
            session_id: SessionId(session_id.to_owned()),
            text: text.to_owned(),
        },
    )
}

fn local_session_id(target: &Target) -> Result<&str, CliError> {
    if target.is_local() {
        Ok(&target.session_id)
    } else {
        Err(CliError::RemoteNotSupported {
            host: target.host_or_local().to_owned(),
        })
    }
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
        output.push_str(&format!("  worktree: {}{branch}\n", path.display()));
    }
    for warning in &info.warnings {
        output.push_str(&format!(
            "  warning [{}]: {}\n",
            warning_kind_label(warning.kind),
            warning.message
        ));
    }
    output
}

fn render_list_human(sessions: &[SessionInfo]) -> String {
    let id_width = sessions
        .iter()
        .map(|s| s.id.0.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());
    let agent_width = sessions
        .iter()
        .map(|s| agent_label(s.agent).len())
        .max()
        .unwrap_or(0)
        .max("AGENT".len());
    let branch_width = sessions
        .iter()
        .map(|s| branch_label(s).len())
        .max()
        .unwrap_or(0)
        .max("BRANCH".len());

    let mut output = String::new();
    output.push_str(&format!(
        "{:<id_width$}  {:<agent_width$}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {:<branch_width$}  {:<4}  CWD\n",
        "ID",
        "AGENT",
        "STATE",
        "ACTIVITY",
        "SOURCE",
        "PID",
        "SIZE",
        "BRANCH",
        "WARN",
        id_width = id_width,
        agent_width = agent_width,
        branch_width = branch_width
    ));
    for session in sessions {
        output.push_str(&format!(
            "{:<id_width$}  {:<agent_width$}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {:<branch_width$}  {:<4}  {}\n",
            session.id.0,
            agent_label(session.agent),
            state_label(session.state),
            activity_label_option(session.activity),
            state_source_label(session.state_source),
            session.pid,
            format!("{}x{}", session.cols, session.rows),
            branch_label(session),
            warn_count_label(session),
            session.cwd.display(),
            id_width = id_width,
            agent_width = agent_width,
            branch_width = branch_width
        ));
    }
    output
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

fn render_inspect_human(info: &SessionInfo) -> String {
    let none = || "<none>".to_owned();
    let rows: Vec<(&str, String)> = vec![
        ("id", info.id.0.clone()),
        ("agent", agent_label(info.agent).to_owned()),
        ("cwd", info.cwd.display().to_string()),
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
            if info.native_session_id.is_some() {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
        ),
        (
            "repo",
            info.repo
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(none),
        ),
        ("branch", info.branch.clone().unwrap_or_else(none)),
        (
            "worktree_path",
            info.worktree_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(none),
        ),
        ("warnings", warn_count_label(info)),
        ("created_at", info.created_at.clone()),
        ("updated_at", info.updated_at.clone()),
        (
            "exit_code",
            info.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(none),
        ),
    ];
    let width = rows
        .iter()
        .map(|(field, _)| field.len())
        .max()
        .unwrap_or(0)
        .max("FIELD".len());

    let mut output = String::new();
    output.push_str(&format!("{:<width$}  VALUE\n", "FIELD", width = width));
    for (field, value) in &rows {
        output.push_str(&format!("{field:<width$}  {value}\n", width = width));
    }
    // Each non-fatal warning is detailed below the table so a worktree session
    // surfaces exactly what happened and what was done instead.
    for warning in &info.warnings {
        output.push_str(&format!(
            "warning [{}]: {}\n",
            warning_kind_label(warning.kind),
            warning.message
        ));
        if let Some(detail) = &warning.detail {
            output.push_str(&format!("  detail: {detail}\n"));
        }
    }
    output
}

fn render_stop_human(session_id: &str, result: &SessionStopResult) -> String {
    format!("session {session_id}: stopped={}\n", result.stopped)
}

fn render_input_human(session_id: &str, result: &SessionInputResult) -> String {
    if result.accepted {
        format!("session {session_id}: input accepted\n")
    } else {
        format!("session {session_id}: input rejected\n")
    }
}

fn agent_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
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

fn activity_label_option(activity: Option<AgentActivity>) -> &'static str {
    activity.map(activity_label).unwrap_or("-")
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
    }
}

fn warning_kind_label(kind: SessionWarningKind) -> &'static str {
    match kind {
        SessionWarningKind::Fetch => "fetch",
        SessionWarningKind::BaseBranchFallback => "base_branch_fallback",
        SessionWarningKind::SetupScript => "setup_script",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use protocol::{
        method, AgentActivity, Request, SessionInfo, SessionState, SessionWarning,
        SessionWarningKind, StateSource,
    };
    use serde_json::json;

    use super::*;
    use crate::error::CliError;
    use crate::target::Target;

    fn running_session(id: &str) -> SessionInfo {
        SessionInfo {
            id: protocol::SessionId(id.to_owned()),
            agent: protocol::AgentKind::Shell,
            cwd: PathBuf::from("/workspace/project"),
            pid: 4242,
            cols: 120,
            rows: 40,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            native_session_id: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            created_at: "2026-06-17T10:00:00Z".to_owned(),
            updated_at: "2026-06-17T10:01:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn new_args(agent: AgentArg, cwd: Option<PathBuf>) -> NewArgs {
        NewArgs {
            agent,
            cwd,
            cols: 80,
            rows: 24,
            repo: None,
            branch: None,
            base_branch: None,
        }
    }

    fn assert_request(request: &Request, method_name: &str, params: serde_json::Value) {
        let value = serde_json::to_value(request).expect("serialize request");
        assert_eq!(
            value,
            json!({
                "v": 1,
                "id": format!("cli-{method_name}"),
                "method": method_name,
                "params": params
            })
        );
    }

    #[test]
    fn new_request_defaults_to_shell_size_and_omits_cwd() {
        let request = build_new_request(&new_args(AgentArg::Shell, None)).expect("request");

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
        let request = build_new_request(&new_args(AgentArg::Codex, None)).expect("request");

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
        let request = build_new_request(&new_args(AgentArg::Claude, None)).expect("request");

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
            agent: AgentArg::Claude,
            cwd: None,
            cols: 80,
            rows: 24,
            repo: Some(PathBuf::from("/workspace/project")),
            branch: Some("feature/login".to_owned()),
            base_branch: Some("main".to_owned()),
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
            agent: AgentArg::Shell,
            cwd: Some(PathBuf::from("/workspace/project")),
            cols: 120,
            rows: 40,
            repo: None,
            branch: None,
            base_branch: None,
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
    fn list_request_uses_null_params() {
        let request = build_list_request();

        assert_request(&request, method::SESSION_LIST, serde_json::Value::Null);
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
    fn input_request_rejects_remote_target() {
        let target: Target = "host-b/s-42".parse().expect("target");

        let err =
            build_input_request(&target, "write tests first").expect_err("remote target must fail");

        match err {
            CliError::RemoteNotSupported { host } => assert_eq!(host, "host-b"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn inspect_request_sends_only_local_session_id() {
        let target: Target = "local/s-42".parse().expect("target");
        let request = build_inspect_request(&target).expect("request");

        assert_request(&request, method::SESSION_INSPECT, json!("s-42"));
    }

    #[test]
    fn stop_request_rejects_remote_target() {
        let target: Target = "host-b/s-42".parse().expect("target");

        let err = build_stop_request(&target).expect_err("remote target must fail");

        match err {
            CliError::RemoteNotSupported { host } => assert_eq!(host, "host-b"),
            other => panic!("unexpected error: {other:?}"),
        }
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
        for column in ["ID", "AGENT", "STATE", "BRANCH", "WARN", "CWD"] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        assert_eq!(
            list_row(&output, "s-42"),
            vec![
                "s-42",
                "shell",
                "running",
                "-",
                "process",
                "4242",
                "120x40",
                "-",
                "-",
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
                "shell",
                "running",
                "working",
                "osc_title",
                "4242",
                "120x40",
                "-",
                "-",
                "/workspace/project",
            ]
        );
    }

    #[test]
    fn renders_codex_and_claude_agents_in_session_list_table() {
        let mut codex = running_session("s-codex");
        codex.agent = protocol::AgentKind::Codex;
        let mut claude = running_session("s-claude");
        claude.agent = protocol::AgentKind::Claude;

        let output = render_list_human(&[codex, claude]);

        assert_eq!(list_row(&output, "s-codex")[1], "codex");
        assert_eq!(list_row(&output, "s-claude")[1], "claude");
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

        let output = render_list_human(&[session]);
        let row = list_row(&output, "s-42");
        // Columns: ID AGENT STATE ACTIVITY SOURCE PID SIZE BRANCH WARN CWD.
        assert_eq!(row[7], "feature/login", "branch column: {row:?}");
        assert_eq!(row[8], "1", "warning-count column: {row:?}");
        assert_eq!(
            row[9],
            "/data/worktrees/s-42-project-feature-login",
            "cwd is the worktree path: {row:?}"
        );
    }

    /// Whether the rendered field/value table contains a `field value` row,
    /// tolerant of the column width (which the longest field name sets).
    fn has_row(output: &str, field: &str, value: &str) -> bool {
        output.lines().any(|line| {
            let mut parts = line.split_whitespace();
            parts.next() == Some(field)
                && parts.next() == Some(value)
                && parts.next().is_none()
        })
    }

    #[test]
    fn renders_inspect_field_value_table() {
        let output = render_inspect_human(&running_session("s-42"));

        assert!(has_row(&output, "FIELD", "VALUE"));
        assert!(has_row(&output, "id", "s-42"));
        assert!(has_row(&output, "agent", "shell"));
        assert!(has_row(&output, "state", "running"));
        assert!(has_row(&output, "activity", "-"));
        assert!(has_row(&output, "state_source", "process"));
        assert!(has_row(&output, "native_session_id", "<none>"));
        assert!(has_row(&output, "resumable", "no"));
        assert!(has_row(&output, "exit_code", "<none>"));
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
    fn renders_claude_agent_in_session_inspect_table() {
        let mut session = running_session("s-42");
        session.agent = protocol::AgentKind::Claude;

        let output = render_inspect_human(&session);

        assert!(has_row(&output, "agent", "claude"));
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
            output.contains("worktree: /data/worktrees/s-42-project-feature-login (branch feature/login)"),
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
}
