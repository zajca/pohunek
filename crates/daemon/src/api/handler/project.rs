//! `project.*` methods plus the shared project-manager and blocking helpers.
//!
//! Project operations touch the git working copy and the metadata store, so all
//! of them are pushed off the async runtime via [`run_project_blocking`]. The
//! registry owns the [`ProjectManager`]; [`require_projects`] surfaces the typed
//! error when the daemon runs without a metadata store.

use std::path::Path;
use std::sync::Arc;

use protocol::{
    CandidateSource, ProjectActionParams, ProjectActionsParams, ProjectActionsResult,
    ProjectAddParams, ProjectCandidatesParams, ProjectListParams, ProjectPromptParams,
    ProjectRemoveParams, ProjectRenameParams, ProjectShowParams, ProtocolError, Request, Response,
};

use super::util::{error_value, ok_value, parse_optional_params, parse_params};
use super::{ControlTransport, DaemonState};
use crate::project::candidates::{self, Inputs, OwnedSession};
use crate::project::{LiveSession, ProjectConfigResolver, ProjectManager};
use crate::session::SessionRegistry;

/// Resolve the project manager, or a typed error response when the daemon has no
/// metadata store configured (so projects are unavailable).
fn require_projects(
    request: &Request,
    sessions: &SessionRegistry,
) -> Result<Arc<ProjectManager>, Response> {
    sessions.projects().ok_or_else(|| {
        error_value(
            request,
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
        Err(err) => return error_value(request, err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    let ProjectListParams {
        filters,
        limit,
        cursor,
    } = params;
    if limit == Some(0) {
        return error_value(
            request,
            ProtocolError::bad_request("project.list limit must be at least 1"),
        );
    }
    let after = match cursor.as_deref().map(split_list_cursor).transpose() {
        Ok(after) => after,
        Err(err) => return error_value(request, err),
    };
    super::util::run_blocking_bounded(
        request,
        move || {
            let mut projects = pm.list(&filters)?;
            // The list is ordered by label, then id: a page is the run after
            // the cursor key.
            if let Some((label, id)) = &after {
                projects.retain(|project| (&project.label, &project.id) > (label, id));
            }
            if let Some(limit) = limit {
                projects.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            }
            Ok(projects)
        },
        "project_task_panicked",
        "project operation task panicked",
        ProtocolError::project_list_too_large(),
    )
    .await
}

/// Split a `project.list` cursor into the label and id it was built from.
fn split_list_cursor(cursor: &str) -> Result<(String, String), ProtocolError> {
    cursor
        .split_once('\0')
        .map(|(label, id)| (label.to_owned(), id.to_owned()))
        .ok_or_else(|| ProtocolError::bad_request("project.list cursor is not a project cursor"))
}

/// `project.candidates`: repositories proposed from pohunek sessions and, when
/// asked for, agent history metadata. Read-only; registers nothing.
pub(super) async fn handle_project_candidates(request: &Request, state: &DaemonState) -> Response {
    let sessions = &state.sessions;
    let params = match parse_optional_params::<ProjectCandidatesParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    // Agent history is the owner's: it is read only for a caller on the local
    // control socket, like the profile homes of the integration methods.
    if params.sources.contains(&CandidateSource::History)
        && state.transport == ControlTransport::Remote
    {
        return error_value(
            request,
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                super::integration::PROFILE_LOCAL_ONLY_CODE,
                "the history source reads agent history of the daemon host and is served on the local control socket only",
                Some("run the command on the host that runs the daemon".to_owned()),
            ),
        );
    }
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    // No source named selects the pohunek source alone: agent history is read
    // only when the caller asks for it.
    let wants_pohunek =
        params.sources.is_empty() || params.sources.contains(&CandidateSource::Pohunek);
    let homes = if params.sources.contains(&CandidateSource::History) {
        match sessions.integration_homes(&sessions.search_path_snapshot()) {
            Ok(homes) => Some(homes),
            Err(err) => return error_value(request, err),
        }
    } else {
        None
    };
    let owned: Vec<OwnedSession> = if wants_pohunek {
        sessions
            .list()
            .await
            .into_iter()
            .filter(|info| info.external != Some(true))
            .map(|info| OwnedSession {
                agent: info.agent_base.as_wire().to_owned(),
                cwd: info.cwd,
                updated_at: info.updated_at,
            })
            .collect()
    } else {
        Vec::new()
    };
    let config = sessions.candidate_config().clone();
    let search_path = sessions.search_path_snapshot();
    run_project_blocking(request, move || {
        let started = std::time::Instant::now();
        let deadline = candidates::deadline_from(started, &config);
        let registered = pm
            .list(&[])?
            .into_iter()
            .map(|project| project.git_common_dir)
            .collect();
        let history = match homes {
            Some(homes) => {
                let home = homes.user_home().ok_or_else(|| {
                    ProtocolError::new(
                        protocol::ErrorClass::Daemon,
                        "candidates_home_unavailable",
                        "the daemon environment has no home directory to bound the history scan"
                            .to_owned(),
                        None,
                    )
                })?;
                Some((
                    crate::external::history::scan(&homes, &config, deadline),
                    home,
                ))
            }
            None => None,
        };
        Ok(candidates::resolve(
            Inputs {
                owned,
                history,
                registered,
                search_path,
            },
            &config,
            deadline,
            std::collections::BTreeSet::new(),
        ))
    })
    .await
}

/// `project.add`: register (or re-add) a project by host-local path.
pub(super) async fn handle_project_add(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectAddParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    // The path is host-local; the CLI sends its own cwd for a local `add` with no
    // PATH, so a missing path here is a contract violation, not a default.
    let Some(path) = params.path else {
        return error_value(
            request,
            ProtocolError::bad_request("project.add requires a host-local path"),
        );
    };
    let (name, base_branch) = (params.name, params.base_branch);
    let search_path = sessions.search_path_snapshot();
    run_project_blocking(request, move || {
        pm.add(&path, name, base_branch, &search_path)
    })
    .await
}

/// `project.show`: a project plus its live worktrees, enriched with which ones
/// pohunek owns and which host a live session.
pub(super) async fn handle_project_show(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<ProjectShowParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let pm = match require_projects(request, sessions) {
        Ok(pm) => pm,
        Err(resp) => return resp,
    };
    // Snapshot live sessions so `show` can mark which worktrees currently host one.
    let live = live_sessions(sessions.list().await);
    let reference = params.reference;
    let search_path = sessions.search_path_snapshot();
    run_project_blocking(request, move || pm.show(&reference, &live, &search_path)).await
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
        Err(err) => return error_value(request, err),
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
        Err(err) => return error_value(request, err),
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
        Err(err) => return error_value(request, err),
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
        Err(err) => return error_value(request, err),
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
        Err(err) => return error_value(request, err),
    };
    // The registry owns both the project store and the worktree manager, so the
    // prune-then-forget orchestration lives there; this guards the no-store case.
    if sessions.projects().is_none() {
        return error_value(
            request,
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
        Err(err) => error_value(request, err),
    }
}
