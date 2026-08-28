//! `integration.install` and `integration.status` agent hook RPC handlers.

use protocol::{IntegrationInstallParams, IntegrationStatusParams, Request, Response};

use super::util::error_value;

use super::util::{ok_value, parse_optional_params, parse_params};

pub(super) fn handle_integration_install(request: &Request) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    match crate::integration::install(params.agent) {
        Ok(result) => ok_value(request, &result),
        Err(err) => error_value(request, err),
    }
}

pub(super) fn handle_integration_status(request: &Request) -> Response {
    let params = match parse_optional_params::<IntegrationStatusParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    match crate::integration::status(params) {
        Ok(result) => ok_value(request, &result),
        Err(err) => error_value(request, err),
    }
}
