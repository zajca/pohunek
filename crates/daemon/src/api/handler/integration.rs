//! `integration.install`: install the agent `SessionStart` hook on this host.

use protocol::{IntegrationInstallParams, Request, Response};

use super::util::{ok_value, parse_params};

pub(super) fn handle_integration_install(request: &Request) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match crate::integration::install(params.agent) {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}
