//! Control-method dispatch.
//!
//! Parses a newline-delimited JSON request line into a [`protocol::Request`],
//! negotiates the protocol version, dispatches to the method handler, and
//! serializes a [`protocol::Response`] back to a single line.
//!
//! Handles `daemon.health` (milestone 2) and the `session.*` lifecycle methods
//! (milestone 3); a `subscribe` request is dispatched specially so the caller can
//! turn the connection into a one-way event stream. Unknown methods get a typed
//! `method_not_found` error so older daemons degrade predictably as the CLI gains
//! methods.

use protocol::{
    method, negotiate, AssistantMaterializeParams, AssistantMaterializeResult, HostDiscoverParams,
    IntegrationInstallParams, ProjectActionParams, ProjectActionsParams, ProjectActionsResult,
    ProjectAddParams, ProjectListParams, ProjectPromptParams, ProjectRemoveParams,
    ProjectRenameParams, ProjectShowParams, ProtocolError, Request, Response, SessionAttachParams,
    SessionDetachParams, SessionId, SessionInputParams, SessionListParams, SessionNewParams,
    SessionNewResult, SessionRenameParams, SessionReportNativeIdParams, SessionResizeParams,
    SessionResumeResult, SessionSetMetadataParams, WorktreeRemoveParams, PROTOCOL_VERSION,
};
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::discovery::DiscoveryCache;
use crate::project::{LiveSession, ProjectConfigResolver, ProjectManager};
use crate::session::SessionRegistry;

/// Static facts the daemon reports from `daemon.health`.
///
/// Cloned into each connection task. Cheap to clone (two short strings).
#[derive(Debug, Clone)]
pub struct HealthInfo {
    /// Daemon build version (e.g. crate version).
    pub daemon_version: String,
}

/// Shared daemon state available to every control connection.
#[derive(Debug, Clone)]
pub struct DaemonState {
    /// Static health metadata.
    pub health: HealthInfo,
    /// In-memory session registry.
    pub sessions: SessionRegistry,
    /// TTL-cached `NetBird` host discovery, shared across connections.
    pub discovery: DiscoveryCache,
}

impl DaemonState {
    /// Construct shared daemon state.
    #[must_use]
    pub fn new(health: HealthInfo, sessions: SessionRegistry) -> Self {
        Self {
            health,
            sessions,
            discovery: DiscoveryCache::default(),
        }
    }
}

impl HealthInfo {
    /// Construct health info from a daemon version string.
    #[must_use]
    pub fn new(daemon_version: impl Into<String>) -> Self {
        Self {
            daemon_version: daemon_version.into(),
        }
    }
}

/// Outcome of dispatching one request line.
///
/// Most requests are one-shot (`Reply`), but a `subscribe` request asks the
/// connection to become a one-way event stream after an OK ack (`Subscribe`).
#[derive(Debug)]
pub(crate) enum Dispatch {
    /// One-shot: send this response line, then keep reading requests.
    Reply(String),
    /// The client asked to subscribe; send this OK ack line, then the caller
    /// streams session events on this connection until the client disconnects.
    Subscribe(String),
    /// The client sent an attach prelude; the caller switches to raw PTY bytes.
    Attach(String),
}

/// Parse one request line and decide how the connection should proceed.
///
/// Never panics and never returns an error: malformed input and version
/// mismatches are turned into typed error responses (`Reply`) so the connection
/// can stay open for the next request. A valid `subscribe` request yields
/// `Subscribe` with the OK ack line for the caller to write before streaming.
pub(crate) async fn dispatch_line(line: &str, state: &DaemonState) -> Dispatch {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Tolerate blank keep-alive lines; reply with a framing error tied to a
        // synthetic id so the client still gets a parseable line.
        let resp = Response::err("", ProtocolError::bad_request("empty request line"));
        return Dispatch::Reply(serialize_response(&resp));
    }

    if let Some(stream_id) = parse_attach_prelude(trimmed) {
        return Dispatch::Attach(stream_id);
    }

    let request: Request = match serde_json::from_str(trimmed) {
        Ok(req) => req,
        Err(err) => {
            warn!(error = %err, "failed to parse control request");
            // We cannot recover the request id from unparseable JSON; use empty.
            let resp = Response::err(
                "",
                ProtocolError::bad_request(format!("invalid request JSON: {err}")),
            );
            return Dispatch::Reply(serialize_response(&resp));
        }
    };

    // Version negotiation first, before treating `subscribe` specially: an
    // incompatible client gets a typed error rather than a long-lived stream.
    if let Err(err) = negotiate(request.v, PROTOCOL_VERSION) {
        let resp = Response::err(request.id.clone(), err);
        return Dispatch::Reply(serialize_response(&resp));
    }

    if request.method == method::SUBSCRIBE {
        let ack = Response::ok(request.id.clone(), json!({ "subscribed": true }));
        return Dispatch::Subscribe(serialize_response(&ack));
    }

    let resp = handle_request(&request, state).await;
    Dispatch::Reply(serialize_response(&resp))
}

/// Dispatch a parsed request to its method handler.
///
/// Exposed within the crate (and re-exported) so integration tests can exercise
/// dispatch without a live socket.
#[must_use]
pub async fn handle_request(request: &Request, state: &DaemonState) -> Response {
    debug!(id = %request.id, method = %request.method, "control request");

    // Version negotiation first: an incompatible client gets a typed error
    // rather than a confusingly-shaped success.
    if let Err(err) = negotiate(request.v, PROTOCOL_VERSION) {
        return Response::err(request.id.clone(), err);
    }

    match request.method.as_str() {
        method::DAEMON_HEALTH => handle_health(request, &state.health),
        method::SESSION_NEW => handle_session_new(request, &state.sessions).await,
        method::SESSION_LIST => handle_session_list(request, &state.sessions).await,
        method::SESSION_INSPECT => handle_session_inspect(request, &state.sessions).await,
        method::SESSION_STOP => handle_session_stop(request, &state.sessions).await,
        method::SESSION_RESUME => handle_session_resume(request, &state.sessions).await,
        method::SESSION_REMOVE => handle_session_remove(request, &state.sessions).await,
        method::SESSION_ATTACH => handle_session_attach(request, &state.sessions).await,
        method::SESSION_DETACH => handle_session_detach(request, &state.sessions).await,
        method::SESSION_RESIZE => handle_session_resize(request, &state.sessions).await,
        method::SESSION_SET_METADATA => handle_session_set_metadata(request, &state.sessions).await,
        method::SESSION_RENAME => handle_session_rename(request, &state.sessions).await,
        method::SESSION_INPUT => handle_session_input(request, &state.sessions).await,
        method::SESSION_REPORT_NATIVE_ID => {
            handle_session_report_native_id(request, &state.sessions).await
        }
        method::DAEMON_DOCTOR => handle_daemon_doctor(request).await,
        method::ASSISTANT_MATERIALIZE => handle_assistant_materialize(request).await,
        method::INTEGRATION_INSTALL => handle_integration_install(request),
        method::HOST_INSPECT => handle_host_inspect(request, &state.health, &state.sessions),
        method::HOST_DISCOVER => handle_host_discover(request, &state.discovery).await,
        method::PROJECT_LIST => handle_project_list(request, &state.sessions).await,
        method::PROJECT_ADD => handle_project_add(request, &state.sessions).await,
        method::PROJECT_SHOW => handle_project_show(request, &state.sessions).await,
        method::PROJECT_RENAME => handle_project_rename(request, &state.sessions).await,
        method::PROJECT_REMOVE => handle_project_remove(request, &state.sessions).await,
        method::PROJECT_PROMPT => handle_project_prompt(request, &state.sessions).await,
        method::PROJECT_ACTION => handle_project_action(request, &state.sessions).await,
        method::PROJECT_ACTIONS => handle_project_actions(request, &state.sessions).await,
        method::WORKTREE_REMOVE => handle_worktree_remove(request, &state.sessions).await,
        other => Response::err(request.id.clone(), ProtocolError::method_not_found(other)),
    }
}

/// `daemon.health`: report daemon version + protocol version.
fn handle_health(request: &Request, health: &HealthInfo) -> Response {
    Response::ok(
        request.id.clone(),
        json!({
            "status": "ok",
            "daemon_version": health.daemon_version,
            "protocol_version": PROTOCOL_VERSION,
        }),
    )
}

/// `host.inspect`: report this host's live capability snapshot.
///
/// The snapshot is built fresh on each request (agent runtimes are probed
/// against `PATH`), so it always reflects the host as it is now. Transport
/// agnostic: the same handler answers over the local Unix socket and over a
/// `NetBird` TCP connection.
fn handle_host_inspect(
    request: &Request,
    health: &HealthInfo,
    sessions: &SessionRegistry,
) -> Response {
    ok_value(
        request,
        &crate::capabilities::host_capabilities(&health.daemon_version, sessions.profiles()),
    )
}

/// `host.discover`: enumerate `NetBird` peers and classify each daemon.
///
/// The probe is run inside the daemon and cached for a short TTL (see
/// [`DiscoveryCache`]), so repeated calls — e.g. every launcher keypress —
/// return the cached snapshot instantly; `force` bypasses the cache and
/// re-probes now. A NetBird-state failure surfaces as a typed
/// `discovery/netbird_state_unavailable` error rather than an empty result.
async fn handle_host_discover(request: &Request, discovery: &DiscoveryCache) -> Response {
    let params = match parse_optional_params::<HostDiscoverParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match discovery.records(params.force).await {
        Ok(records) => ok_value(request, &records),
        Err(err) => Response::err(
            request.id.clone(),
            ProtocolError::netbird_state_unavailable(err.to_string()),
        ),
    }
}

/// Resolve the project manager, or a typed error response when the daemon has no
/// metadata store configured (so projects are unavailable).
fn require_projects(
    request: &Request,
    sessions: &SessionRegistry,
) -> Result<Arc<ProjectManager>, Response> {
    sessions.projects().ok_or_else(|| {
        Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "projects_not_configured",
                "the daemon is not configured for projects (no metadata store)".to_owned(),
                None,
            ),
        )
    })
}

/// Run a fallible blocking operation off the async runtime and map its result to
/// a [`Response`]: a serialized value on success, the operation's typed error on
/// failure, and a daemon-class error built from `panic_code`/`panic_msg`/
/// `panic_hint` if the `spawn_blocking` task panics (the `JoinError` case).
async fn run_blocking<T, F>(
    request: &Request,
    op: F,
    panic_code: &'static str,
    panic_msg: &'static str,
    panic_hint: Option<&'static str>,
) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    match tokio::task::spawn_blocking(op).await {
        Ok(Ok(value)) => ok_value(request, &value),
        Ok(Err(err)) => Response::err(request.id.clone(), err),
        Err(_) => Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                panic_code,
                panic_msg,
                panic_hint.map(str::to_owned),
            ),
        ),
    }
}

/// Run a blocking project operation off the async runtime and build its response.
async fn run_project_blocking<T, F>(request: &Request, op: F) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    run_blocking(
        request,
        op,
        "project_task_panicked",
        "project operation task panicked",
        None,
    )
    .await
}

/// `project.list`: known projects on this host, AND-filtered.
async fn handle_project_list(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_optional_params::<ProjectListParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    let filters = params.filters;
    run_project_blocking(request, move || pm.list(&filters)).await
}

/// `project.add`: register (or re-add) a project by host-local path.
async fn handle_project_add(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectAddParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    // The path is host-local; the CLI sends its own cwd for a local `add` with no
    // PATH, so a missing path here is a contract violation, not a default.
    let Some(path) = params.path else {
        return Response::err(
            request.id.clone(),
            ProtocolError::bad_request("project.add requires a host-local path"),
        );
    };
    let (name, base_branch) = (params.name, params.base_branch);
    run_project_blocking(request, move || pm.add(&path, name, base_branch)).await
}

/// Build the live-session snapshot `project show` consumes from the registry's
/// session list: drop terminal sessions, then project each survivor to the
/// path-only [`LiveSession`] shape.
///
/// The registry retains terminal sessions — `record_exit` flips the state, it
/// does not evict the entry — so without this filter a stopped/done/failed
/// session would keep marking its worktree as occupied in `project show`. Only
/// non-terminal (`Starting`/`Running`) sessions actually hold a worktree.
fn live_sessions(infos: Vec<protocol::SessionInfo>) -> Vec<LiveSession> {
    infos
        .into_iter()
        .filter(|session| !session.state.is_terminal())
        .map(|session| LiveSession {
            session_id: session.id.0,
            cwd: session.cwd,
            worktree_path: session.worktree_path,
        })
        .collect()
}

/// `project.show`: a project plus its live worktrees, enriched with which ones
/// pohunek owns and which host a live session.
async fn handle_project_show(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectShowParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    // Snapshot live sessions so `show` can mark which worktrees currently host one.
    let live = live_sessions(sessions.list().await);
    let reference = params.reference;
    run_project_blocking(request, move || pm.show(&reference, &live)).await
}

/// `project.prompt`: resolve one prompt by name to its template content,
/// fail-closed (`prompt_not_found`). The primitive behind `project.action` and the
/// `pohunek project prompt` command. Read-only — it does not bump `last_used`.
async fn handle_project_prompt(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectPromptParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    // The host-default layer lives under the daemon's config dir; `None` disables it
    // (the resolver then sees in-repo only). The daemon reads its own host's
    // `.pohunek/`, so this works identically for a local or a remote project.
    let config_dir = sessions.config_dir().map(Path::to_path_buf);
    let ProjectPromptParams { reference, name } = params;
    run_project_blocking(request, move || {
        let record = pm.resolve(&reference)?;
        let resolver = ProjectConfigResolver::new(record.repo_root, config_dir);
        resolver.resolve_prompt(&name)
    })
    .await
}

/// `project.action`: resolve one action to its full recipe (provider, agent, base
/// branch, branch rule, prompt name + resolved prompt content). The command the
/// launcher calls. Read-only — does not bump `last_used`.
async fn handle_project_action(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectActionParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    let config_dir = sessions.config_dir().map(Path::to_path_buf);
    let ProjectActionParams { reference, name } = params;
    run_project_blocking(request, move || {
        let record = pm.resolve(&reference)?;
        let resolver = ProjectConfigResolver::new(record.repo_root, config_dir);
        resolver.resolve_action(&name)
    })
    .await
}

/// `project.actions`: list the actions resolvable for a project (the union across
/// the in-repo and host layers), with the template each uses.
async fn handle_project_actions(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectActionsParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    let config_dir = sessions.config_dir().map(Path::to_path_buf);
    let ProjectActionsParams { reference } = params;
    run_project_blocking(request, move || {
        let record = pm.resolve(&reference)?;
        let resolver = ProjectConfigResolver::new(record.repo_root, config_dir);
        resolver
            .list_actions()
            .map(|actions| ProjectActionsResult { actions })
    })
    .await
}

/// `project.rename`: set a project's custom display name.
async fn handle_project_rename(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectRenameParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    let (reference, name) = (params.reference, params.name);
    run_project_blocking(request, move || pm.rename(&reference, name)).await
}

/// `project.remove`: forget a project record, optionally pruning pohunek-owned
/// worktrees for it (`--prune-worktrees`); never touches the main checkout or
/// unowned worktrees.
async fn handle_project_remove(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectRemoveParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    // The registry owns both the project store and the worktree manager, so the
    // prune-then-forget orchestration lives there; this guards the no-store case.
    if sessions.projects().is_none() {
        return Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "projects_not_configured",
                "the daemon is not configured for projects (no metadata store)".to_owned(),
                None,
            ),
        );
    }
    match sessions
        .remove_project(&params.reference, params.prune_worktrees)
        .await
    {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

/// `worktree.remove`: remove a single pohunek-owned worktree by path. Fail-closed
/// — refuses an external (unowned) worktree (`worktree_not_owned`) and one a live
/// session still uses (`worktree_in_use`); never touches the main checkout.
async fn handle_worktree_remove(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<WorktreeRemoveParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.remove_worktree(&params.path).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_new(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionNewParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    // `create` only returns `Ok` after a requested initial input was injected
    // (it rolls back and errors otherwise), so a successful create with input
    // set means the input was applied. Echoing this lets a client detect an
    // older daemon that silently ignored `input` (which returns no flag).
    let requested_input = params.input.is_some();
    match sessions.create(params).await {
        Ok(session) => {
            let result = SessionNewResult {
                session,
                applied_input: requested_input.then_some(true),
            };
            ok_value(request, &result)
        }
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_list(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_optional_params::<SessionListParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let mut list = sessions.list().await;
    if !params.filters.is_empty() {
        list.retain(|session| params.filters.iter().all(|filter| filter.matches(session)));
    }
    ok_value(request, &list)
}

async fn handle_session_inspect(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.inspect(&id).await {
        Ok(info) => ok_value(request, &info),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_stop(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.stop(&id).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_resume(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.resume(&id).await {
        Ok(session) => ok_value(request, &SessionResumeResult { session }),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_remove(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.remove(&id).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_attach(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionAttachParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.attach(&params).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_detach(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionDetachParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.detach(&params.stream_id).await;
    ok_value(request, &result)
}

async fn handle_session_resize(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionResizeParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions
        .resize(&params.session_id, params.cols, params.rows)
        .await
    {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_set_metadata(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionSetMetadataParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions
        .set_metadata(&params.session_id, params.metadata)
        .await
    {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_rename(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionRenameParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.rename(&params.session_id, params.name).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_input(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionInputParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.input(params).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_report_native_id(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionReportNativeIdParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.report_native_id(params).await;
    ok_value(request, &result)
}

fn handle_integration_install(request: &Request) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match crate::integration::install(params.agent) {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_daemon_doctor(request: &Request) -> Response {
    if !request.params.is_null() {
        return Response::err(
            request.id.clone(),
            ProtocolError::bad_request("daemon.doctor does not accept params"),
        );
    }
    let paths = match crate::Paths::resolve() {
        Ok(paths) => paths,
        Err(err) => {
            return Response::err(
                request.id.clone(),
                ProtocolError::new(
                    protocol::ErrorClass::Configuration,
                    "paths_unavailable",
                    format!("failed to resolve daemon paths: {err}"),
                    Some("set the required XDG environment variables and retry".to_owned()),
                ),
            );
        }
    };
    match tokio::task::spawn_blocking(move || crate::doctor::report(&paths)).await {
        Ok(report) => ok_value(request, &protocol::DaemonDoctorResult { report }),
        Err(_) => Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "doctor_task_panicked",
                "daemon doctor task panicked".to_owned(),
                Some("retry the request; if it repeats, inspect daemon logs".to_owned()),
            ),
        ),
    }
}

async fn handle_assistant_materialize(request: &Request) -> Response {
    let params = match parse_params::<AssistantMaterializeParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let paths = match crate::Paths::resolve() {
        Ok(paths) => paths,
        Err(err) => {
            return Response::err(
                request.id.clone(),
                ProtocolError::materialization_failed("assistant paths", &err.to_string()),
            );
        }
    };

    let snapshot = params.snapshot;
    run_assistant_materialize_blocking(request, move || {
        crate::assistant::materialize_assistant(&paths, &snapshot)
    })
    .await
}

async fn run_assistant_materialize_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<AssistantMaterializeResult, ProtocolError> + Send + 'static,
{
    run_blocking(
        request,
        op,
        "assistant_materialize_task_panicked",
        "assistant materialization task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
}

fn parse_params<T>(request: &Request) -> Result<T, ProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value::<T>(request.params.clone()).map_err(|err| {
        ProtocolError::bad_request(format!("invalid params for {}: {err}", request.method))
    })
}

fn parse_optional_params<T>(request: &Request) -> Result<T, ProtocolError>
where
    T: serde::de::DeserializeOwned + Default,
{
    if request.params.is_null() {
        Ok(T::default())
    } else {
        parse_params(request)
    }
}

fn ok_value<T>(request: &Request, value: &T) -> Response
where
    T: Serialize,
{
    match serde_json::to_value(value) {
        Ok(value) => Response::ok(request.id.clone(), value),
        Err(err) => Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "serialize_failed",
                format!("failed to serialize response: {err}"),
                None,
            ),
        ),
    }
}

/// Serialize a response to a single JSON line.
///
/// Serialization of our own typed envelopes cannot fail in practice; if it ever
/// did we fall back to a minimal hand-built error line rather than panicking.
pub(crate) fn serialize_response(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|err| {
        warn!(error = %err, "failed to serialize response; sending fallback error");
        format!(
            r#"{{"v":{},"id":"{}","err":{{"class":"daemon","code":"serialize_failed","msg":"response serialization failed"}}}}"#,
            PROTOCOL_VERSION.get(),
            resp.id().replace('"', "")
        )
    })
}

fn parse_attach_prelude(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }

    match object.get("attach") {
        Some(Value::String(stream_id)) if !stream_id.is_empty() => Some(stream_id.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use protocol::{
        method, AgentKind, AssistantMaterializeParams, AssistantMaterializeResult,
        DaemonDoctorResult, Request, SessionId, SessionInfo, SessionNewParams,
        SessionSetMetadataParams, SessionSetMetadataResult, SessionState, StateSource,
    };

    use super::{handle_request, live_sessions, parse_attach_prelude, DaemonState, HealthInfo};
    use crate::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};

    /// A minimal `SessionInfo` for the given id/state with a worktree path, so a
    /// test can assert which sessions survive the `project show` live filter.
    fn session(id: &str, state: SessionState) -> SessionInfo {
        let path = PathBuf::from(format!("/work/{id}"));
        SessionInfo {
            id: SessionId(id.to_owned()),
            name: None,
            agent: "shell".to_owned(),
            agent_base: AgentKind::Shell,
            cwd: path.clone(),
            pid: 0,
            cols: 80,
            rows: 24,
            state,
            state_source: StateSource::Process,
            activity: None,
            native_session_id: None,
            native_session_path: None,
            project_id: None,
            project_label: None,
            metadata: BTreeMap::new(),
            is_linked_worktree: Some(true),
            repo: None,
            branch: None,
            worktree_path: Some(path),
            warnings: Vec::new(),
            created_at: "2026-06-23T00:00:00Z".to_owned(),
            updated_at: "2026-06-23T00:00:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn session_set_metadata_dispatch_updates_session() {
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let created = sessions
            .create(SessionNewParams {
                agent: "shell".to_owned(),
                name: None,
                cwd: Some(PathBuf::from("/tmp")),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                metadata: metadata(&[("owner", "cli")]),
            })
            .await
            .expect("create session");
        let state = DaemonState::new(HealthInfo::new("test"), sessions.clone());
        let params = SessionSetMetadataParams {
            session_id: created.id.clone(),
            metadata: BTreeMap::from([
                ("owner".to_owned(), Some("daemon".to_owned())),
                ("ticket".to_owned(), Some("DMD-1356".to_owned())),
            ]),
        };
        let request = Request::new(
            "set-metadata",
            method::SESSION_SET_METADATA,
            serde_json::to_value(params).expect("params serialize"),
        );

        let response = handle_request(&request, &state).await;

        let protocol::Response::Ok { ok, .. } = response else {
            panic!("expected session.set_metadata ok response: {response:?}");
        };
        let result: SessionSetMetadataResult =
            serde_json::from_value(ok).expect("result deserializes");
        assert_eq!(
            result.session.metadata,
            metadata(&[("owner", "daemon"), ("ticket", "DMD-1356")])
        );
        assert_eq!(
            sessions
                .inspect(&created.id)
                .await
                .expect("inspect")
                .metadata,
            result.session.metadata
        );

        let _ = sessions.stop(&created.id).await;
    }

    #[tokio::test]
    async fn assistant_materialize_persists_snapshot_and_returns_bundle_metadata() {
        let _env = EnvGuard::set_all("assistant-materialize-rpc");
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::new(SessionRegistryConfig::default()),
        );
        let params = AssistantMaterializeParams {
            snapshot: r#"{"daemon":"running"}"#.to_owned(),
        };
        let request = Request::new(
            "assistant-materialize",
            method::ASSISTANT_MATERIALIZE,
            serde_json::to_value(params).expect("params serialize"),
        );

        let response = handle_request(&request, &state).await;

        let protocol::Response::Ok { ok, .. } = response else {
            panic!("expected assistant.materialize ok response: {response:?}");
        };
        let result: AssistantMaterializeResult =
            serde_json::from_value(ok).expect("result deserializes");
        assert!(std::path::Path::new(&result.bundle_path)
            .join("index.md")
            .is_file());
        assert_eq!(
            std::fs::read_to_string(&result.snapshot_path).expect("snapshot"),
            r#"{"daemon":"running"}"#
        );
        assert!(result.content_hash.starts_with("sha256:"));
        assert!(!result.concepts.is_empty());
    }

    #[tokio::test]
    async fn assistant_materialize_uses_unique_snapshot_paths_per_request() {
        let _env = EnvGuard::set_all("assistant-materialize-unique-rpc");
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::new(SessionRegistryConfig::default()),
        );

        let first =
            assistant_materialize_result(&state, "assistant-materialize-first", "first").await;
        let second =
            assistant_materialize_result(&state, "assistant-materialize-second", "second").await;

        assert_eq!(first.bundle_path, second.bundle_path);
        assert_ne!(first.snapshot_path, second.snapshot_path);
        assert_eq!(
            std::fs::read_to_string(&first.snapshot_path).expect("first snapshot"),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(&second.snapshot_path).expect("second snapshot"),
            "second"
        );
    }

    #[tokio::test]
    async fn daemon_doctor_returns_report() {
        let _env = EnvGuard::set_all("daemon-doctor-rpc");
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::new(SessionRegistryConfig::default()),
        );
        let request = Request::new(
            "daemon-doctor",
            method::DAEMON_DOCTOR,
            serde_json::Value::Null,
        );

        let response = handle_request(&request, &state).await;

        let protocol::Response::Ok { ok, .. } = response else {
            panic!("expected daemon.doctor ok response: {response:?}");
        };
        let result: DaemonDoctorResult =
            serde_json::from_value(ok).expect("doctor result deserializes");
        assert!(result
            .report
            .checks
            .iter()
            .any(|check| check.name == "socket_dir_writable"));
    }

    #[tokio::test]
    async fn assistant_materialize_blocking_task_panic_returns_daemon_error() {
        let request = Request::new(
            "assistant-materialize-panic",
            method::ASSISTANT_MATERIALIZE,
            serde_json::json!({ "snapshot": "{}" }),
        );

        let response =
            super::run_assistant_materialize_blocking(&request, || panic!("materialize panic"))
                .await;

        let protocol::Response::Err { err, .. } = response else {
            panic!("expected assistant.materialize error response: {response:?}");
        };
        assert_eq!(err.class, protocol::ErrorClass::Daemon);
        assert_eq!(err.code, "assistant_materialize_task_panicked");
    }

    async fn assistant_materialize_result(
        state: &DaemonState,
        id: &str,
        snapshot: &str,
    ) -> AssistantMaterializeResult {
        let params = AssistantMaterializeParams {
            snapshot: snapshot.to_owned(),
        };
        let request = Request::new(
            id,
            method::ASSISTANT_MATERIALIZE,
            serde_json::to_value(params).expect("params serialize"),
        );
        let response = handle_request(&request, state).await;
        let protocol::Response::Ok { ok, .. } = response else {
            panic!("expected assistant.materialize ok response: {response:?}");
        };
        serde_json::from_value(ok).expect("result deserializes")
    }

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set_all(tag: &str) -> Self {
            let lock = crate::test_support::XDG_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let vars = [
                "XDG_RUNTIME_DIR",
                "XDG_STATE_HOME",
                "XDG_DATA_HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "HOME",
            ];
            let saved = vars
                .iter()
                .map(|&key| (key, std::env::var(key).ok()))
                .collect::<Vec<_>>();
            let root = std::env::temp_dir().join(format!(
                "pohunek-handler-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after unix epoch")
                    .as_nanos()
            ));
            std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
            std::env::set_var("XDG_STATE_HOME", root.join("state"));
            std::env::set_var("XDG_DATA_HOME", root.join("data"));
            std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
            std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
            std::env::set_var("HOME", root.join("home"));
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn live_sessions_keeps_only_non_terminal_sessions() {
        let infos = vec![
            session("starting", SessionState::Starting),
            session("running", SessionState::Running),
            session("stopped", SessionState::Stopped),
            session("done", SessionState::Done),
            session("failed", SessionState::Failed),
        ];

        let live = live_sessions(infos);

        // Starting + Running survive; the three terminal states are dropped, so a
        // stopped session can no longer mark its worktree as occupied in `show`.
        let ids: Vec<&str> = live.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["starting", "running"]);
    }

    #[test]
    fn attach_prelude_requires_exact_one_field_shape() {
        assert_eq!(
            parse_attach_prelude(r#"{"attach":"a-1"}"#),
            Some("a-1".to_owned())
        );
        assert_eq!(parse_attach_prelude(r#"{"attach":""}"#), None);
        assert_eq!(
            parse_attach_prelude(
                r#"{"v":1,"id":"req-1","method":"daemon.health","params":null,"attach":"a-1"}"#
            ),
            None
        );
    }
}
