//! `command.run`: run a command an installed package declared.
//!
//! Served on the local socket and on remote overlay connections. Whether a
//! command may run for a remote caller is the command's declared exposure,
//! decided by the service from the connection's transport.

// Rust guideline compliant 2026-10-28

use protocol::{CommandRunParams, Request, Response};

use super::util::{error_value, ok_value, parse_params};
use super::DaemonState;

pub(super) async fn handle_command_run(request: &Request, state: &DaemonState) -> Response {
    let params = match parse_params::<CommandRunParams>(request) {
        Ok(params) => params,
        Err(error) => return error_value(request, error),
    };
    match state.package_commands.run(params, state.transport).await {
        Ok(result) => ok_value(request, &result),
        Err(error) => error_value(request, error),
    }
}
