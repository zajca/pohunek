//! `store.rejected_list` and `store.rejected_set_aside`: the metadata-store
//! lines no loader accepts.
//!
//! The methods read and move the owner's durable records, so they are served
//! on the local control socket only. Each handler refuses a remote connection
//! before it parses a parameter or touches the store.

// Rust guideline compliant 2026-10-29

use protocol::{PackageErrorKind, ProtocolError, Request, Response, StoreRejectedSetAsideParams};

use super::util::{error_value, ok_value, parse_params};
use super::{ControlTransport, DaemonState};

/// Refuses a request that did not arrive on the local control socket.
fn require_local(request: &Request, state: &DaemonState) -> Result<(), Response> {
    match state.transport {
        ControlTransport::Local => Ok(()),
        ControlTransport::Remote => Err(error_value(
            request,
            ProtocolError::from(PackageErrorKind::LocalOnly),
        )),
    }
}

pub(super) async fn handle_store_rejected_list(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match state.sessions.store_rejected_list().await {
        Ok(result) => ok_value(request, &result),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_store_rejected_set_aside(
    request: &Request,
    state: &DaemonState,
) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<StoreRejectedSetAsideParams>(request) {
        Ok(params) => match state.sessions.store_rejected_set_aside(params).await {
            Ok(result) => ok_value(request, &result),
            Err(error) => error_value(request, error),
        },
        Err(error) => error_value(request, error),
    }
}
