//! `zagentmesh session` — manage local PTY-backed sessions.
//!
//! Milestone 3 only supports the local transport and the `shell` agent. The CLI
//! grammar is host-aware through [`crate::target::Target`], but remote targets
//! are rejected before any daemon request is sent.

use std::path::PathBuf;

use clap::ValueEnum;
use protocol::{
    AgentActivity, AgentKind, Request, SessionId, SessionInfo, SessionNewParams, SessionState,
    SessionStopResult, StateSource, method,
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
}

impl From<AgentArg> for AgentKind {
    fn from(value: AgentArg) -> Self {
        match value {
            AgentArg::Shell => AgentKind::Shell,
        }
    }
}

/// Run `session new`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request, or
/// returns a payload that does not match the session contract.
pub(crate) async fn run_new(
    paths: &Paths,
    agent: AgentArg,
    cwd: Option<PathBuf>,
    cols: u16,
    rows: u16,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(&paths.socket).await?;
    let request = build_new_request(agent, cwd, cols, rows)?;
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

fn build_new_request(
    agent: AgentArg,
    cwd: Option<PathBuf>,
    cols: u16,
    rows: u16,
) -> Result<Request, CliError> {
    request_with_params(
        method::SESSION_NEW,
        &SessionNewParams {
            agent: agent.into(),
            cwd,
            cols,
            rows,
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
    format!(
        "session {} created (state: {})\n",
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

    let mut output = String::new();
    output.push_str(&format!(
        "{:<id_width$}  {:<5}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  CWD\n",
        "ID",
        "AGENT",
        "STATE",
        "ACTIVITY",
        "SOURCE",
        "PID",
        "SIZE",
        id_width = id_width
    ));
    for session in sessions {
        output.push_str(&format!(
            "{:<id_width$}  {:<5}  {:<7}  {:<8}  {:<12}  {:<4}  {:<6}  {}\n",
            session.id.0,
            agent_label(session.agent),
            state_label(session.state),
            activity_label_option(session.activity),
            state_source_label(session.state_source),
            session.pid,
            format!("{}x{}", session.cols, session.rows),
            session.cwd.display(),
            id_width = id_width
        ));
    }
    output
}

fn render_inspect_human(info: &SessionInfo) -> String {
    let rows = [
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
        ("created_at", info.created_at.clone()),
        ("updated_at", info.updated_at.clone()),
        (
            "exit_code",
            info.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "<none>".to_owned()),
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
    for (field, value) in rows {
        output.push_str(&format!("{field:<width$}  {value}\n", width = width));
    }
    output
}

fn render_stop_human(session_id: &str, result: &SessionStopResult) -> String {
    format!("session {session_id}: stopped={}\n", result.stopped)
}

fn agent_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use protocol::{AgentActivity, Request, SessionInfo, SessionState, StateSource, method};
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
            created_at: "2026-06-17T10:00:00Z".to_owned(),
            updated_at: "2026-06-17T10:01:00Z".to_owned(),
            exit_code: None,
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
        let request = build_new_request(AgentArg::Shell, None, 80, 24).expect("request");

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
    fn new_request_includes_cwd_and_requested_size() {
        let request = build_new_request(
            AgentArg::Shell,
            Some(PathBuf::from("/workspace/project")),
            120,
            40,
        )
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

    #[test]
    fn renders_compact_session_list_table() {
        let output = render_list_human(&[running_session("s-42")]);

        assert!(
            output.contains("ID    AGENT  STATE    ACTIVITY  SOURCE        PID   SIZE    CWD\n")
        );
        assert!(output.contains(
            "s-42  shell  running  -         process       4242  120x40  /workspace/project\n"
        ));
    }

    #[test]
    fn renders_detected_activity_in_session_list_table() {
        let mut session = running_session("s-42");
        session.activity = Some(AgentActivity::Working);
        session.state_source = StateSource::OscTitle;

        let output = render_list_human(&[session]);

        assert!(output.contains(
            "s-42  shell  running  working   osc_title     4242  120x40  /workspace/project\n"
        ));
    }

    #[test]
    fn renders_inspect_field_value_table() {
        let output = render_inspect_human(&running_session("s-42"));

        assert!(output.contains("FIELD         VALUE\n"));
        assert!(output.contains("id            s-42\n"));
        assert!(output.contains("agent         shell\n"));
        assert!(output.contains("state         running\n"));
        assert!(output.contains("activity      -\n"));
        assert!(output.contains("state_source  process\n"));
        assert!(output.contains("exit_code     <none>\n"));
    }

    #[test]
    fn renders_stop_result_with_target_id() {
        let output = render_stop_human("s-42", &protocol::SessionStopResult { stopped: true });

        assert_eq!(output, "session s-42: stopped=true\n");
    }
}
