//! `session.*` lifecycle methods.
//!
//! Each handler parses its params, delegates to the shared [`SessionRegistry`],
//! and maps the registry's typed result or error onto a [`Response`]. The
//! registry — not this module — owns all session state and locking; these fns
//! are thin transport glue.

use protocol::{
    Request, Response, SessionAttachParams, SessionDetachParams, SessionForkParams,
    SessionForkResult, SessionId, SessionInputParams, SessionListParams, SessionNewParams,
    SessionNewResult, SessionReleaseAgentParams, SessionRenameParams, SessionReportAgentParams,
    SessionReportNativeIdParams, SessionResizeParams, SessionResumeResult,
    SessionSetMetadataParams,
};

use super::util::{ok_value, parse_optional_params, parse_params};
use crate::session::SessionRegistry;

pub(super) async fn handle_session_new(request: &Request, sessions: &SessionRegistry) -> Response {
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

pub(super) async fn handle_session_list(request: &Request, sessions: &SessionRegistry) -> Response {
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

pub(super) async fn handle_session_inspect(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.inspect(&id).await {
        Ok(info) => ok_value(request, &info),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_stop(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.stop(&id).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_resume(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.resume(&id).await {
        Ok(session) => ok_value(request, &SessionResumeResult { session }),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_fork(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionForkParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.fork(params).await {
        Ok(session) => ok_value(
            request,
            &SessionForkResult {
                session,
                applied_input: None,
            },
        ),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_remove(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.remove(&id).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_attach(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionAttachParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.attach(&params).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_detach(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionDetachParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.detach(&params.stream_id).await;
    ok_value(request, &result)
}

pub(super) async fn handle_session_resize(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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

pub(super) async fn handle_session_set_metadata(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
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

pub(super) async fn handle_session_rename(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionRenameParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.rename(&params.session_id, params.name).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_input(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionInputParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.input(params).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

pub(super) async fn handle_session_report_native_id(
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

pub(super) async fn handle_session_report_agent(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionReportAgentParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.report_agent(params).await;
    ok_value(request, &result)
}

pub(super) async fn handle_session_release_agent(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionReleaseAgentParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.release_agent(params).await;
    ok_value(request, &result)
}
