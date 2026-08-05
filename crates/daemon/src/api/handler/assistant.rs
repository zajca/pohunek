//! `assistant.materialize`: build the assistant knowledge bundle host-side.

use protocol::{
    AssistantMaterializeParams, AssistantMaterializeResult, ProtocolError, Request, Response,
};

use super::util::{error_value, parse_params};

pub(super) async fn handle_assistant_materialize(request: &Request) -> Response {
    let params = match parse_params::<AssistantMaterializeParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let paths = match crate::Paths::resolve() {
        Ok(paths) => paths,
        Err(err) => {
            return error_value(
                request,
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

/// Run assistant materialization off the async runtime, mapping a task panic to a
/// typed daemon error. Exposed to the handler tests that assert the panic path.
pub(super) async fn run_assistant_materialize_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<AssistantMaterializeResult, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "assistant_materialize_task_panicked",
        "assistant materialization task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
}
