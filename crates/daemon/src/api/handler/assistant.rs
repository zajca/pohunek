//! `assistant.materialize`: resolve the assistant context package host-side and
//! persist the launch snapshot.

use protocol::{AssistantMaterializeParams, ProtocolError, Request, Response};

use super::util::{error_value, ok_value, parse_params};
use super::DaemonState;

pub(super) async fn handle_assistant_materialize(
    request: &Request,
    state: &DaemonState,
) -> Response {
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
    match crate::assistant::materialize_assistant(&paths, &state.sessions.packages(), params).await
    {
        Ok(result) => ok_value(request, &result),
        Err(error) => error_value(request, error),
    }
}
