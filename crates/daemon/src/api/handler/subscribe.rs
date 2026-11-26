//! The `subscribe` method: parameter validation for the event stream.

use std::time::Duration;

use protocol::{ProtocolError, Request, Response, SubscribeParams};
use serde_json::json;

use super::util::{error_value, parse_optional_params, response_ok};
use crate::session::SessionRegistry;

/// `subscribe`: validate the optional `heartbeat_ms` against the configured
/// bounds and acknowledge. The connection is not subscribed on an error.
pub(super) fn handle_subscribe(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_optional_params::<SubscribeParams>(request) {
        Ok(params) => params,
        Err(error) => return error_value(request, error),
    };
    if let Some(heartbeat_ms) = params.heartbeat_ms {
        let settings = sessions.subscription_settings();
        if !(settings.heartbeat_min_ms..=settings.heartbeat_max_ms).contains(&heartbeat_ms) {
            return error_value(
                request,
                ProtocolError::heartbeat_out_of_range(
                    settings.heartbeat_min_ms,
                    settings.heartbeat_max_ms,
                ),
            );
        }
    }
    response_ok(request, json!({ "subscribed": true }))
}

/// The heartbeat interval an acknowledged `subscribe` request asked for.
pub(super) fn requested_heartbeat(request: &Request) -> Option<Duration> {
    parse_optional_params::<SubscribeParams>(request)
        .ok()
        .and_then(|params| params.heartbeat_ms)
        .map(|millis| Duration::from_millis(u64::from(millis)))
}
