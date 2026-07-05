//! `project.*` methods plus the shared project-manager and blocking helpers.
//!
//! Project operations touch the git working copy and the metadata store, so all
//! of them are pushed off the async runtime via [`run_project_blocking`]. The
//! registry owns the [`ProjectManager`]; [`require_projects`] surfaces the typed
//! error when the daemon runs without a metadata store.

use std::path::Path;
use std::sync::Arc;

use protocol::{
    ProjectActionParams, ProjectActionsParams, ProjectActionsResult, ProjectAddParams,
    ProjectListParams, ProjectPromptParams, ProjectRemoveParams, ProjectRenameParams,
    ProjectShowParams, ProtocolError, Request, Response,
};

use super::util::{ok_value, parse_optional_params, parse_params};
use crate::project::{LiveSession, ProjectConfigResolver, ProjectManager};
use crate::session::SessionRegistry;

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

/// Run a blocking project operation off the async runtime and build its response.
async fn run_project_blocking<T, F>(request: &Request, op: F) -> Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "project_task_panicked",
        "project operation task panicked",
        None,
    )
    .await
}

/// Build the live-session snapshot `project show` consumes from the registry's
/// session list: drop terminal sessions, then project each survivor to the
/// path-only [`LiveSession`] shape.
///
/// The registry retains terminal sessions — `record_exit` flips the state, it
/// does not evict the entry — so without this filter a stopped/done/failed
/// session would keep marking its worktree as occupied in `project show`. Only
/// non-terminal (`Starting`/`Running`) sessions actually hold a worktree.
pub(super) fn live_sessions(infos: Vec<protocol::SessionInfo>) -> Vec<LiveSession> {
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

/// `project.list`: known projects on this host, AND-filtered.
pub(super) async fn handle_project_list(request: &Request, sessions: &SessionRegistry) -> Response {
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
pub(super) async fn handle_project_add(request: &Request, sessions: &SessionRegistry) -> Response {
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

/// `project.show`: a project plus its live worktrees, enriched with which ones
/// pohunek owns and which host a live session.
pub(super) async fn handle_project_show(request: &Request, sessions: &SessionRegistry) -> Response {
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
pub(super) async fn handle_project_prompt(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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
pub(super) async fn handle_project_action(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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
pub(super) async fn handle_project_actions(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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
pub(super) async fn handle_project_rename(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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
pub(super) async fn handle_project_remove(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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
