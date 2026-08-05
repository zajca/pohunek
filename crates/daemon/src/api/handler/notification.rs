//! `notification.*` methods and their durable-store helpers.
//!
//! The durable [`NotificationService`] and the [`AttentionCoordinator`] are
//! optional daemon components; [`require_notifications`]/[`require_attention`]
//! surface the typed error when a request needs one the daemon wasn't built
//! with. Store mutations run off the async runtime via [`run_notification_blocking`].

use protocol::{
    NotificationCreateParams, NotificationCreateResult, NotificationDeleteParams,
    NotificationListParams, NotificationPolicyParams, NotificationPolicyResult,
    NotificationRetentionParams, NotificationUpdateParams, ProtocolError, Request, Response,
};

use super::util::{error_value, ok_value, parse_optional_params, parse_params};
use crate::notifications::{is_debounced_create, AttentionCoordinator, NotificationService};
use crate::session::SessionRegistry;

/// Resolve the notification service, or a typed error response when this daemon
/// state was built without durable notification storage.
fn require_notifications(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Result<NotificationService, Response> {
    notifications.cloned().ok_or_else(|| {
        error_value(
            request,
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "notifications_not_configured",
                "the daemon is not configured for notifications".to_owned(),
                None,
            ),
        )
    })
}

/// Resolve the session notification debounce coordinator.
///
/// Returns a typed error response when this daemon state was built with
/// notifications but no coordinator.
fn require_attention(
    request: &Request,
    attention: Option<&AttentionCoordinator>,
) -> Result<AttentionCoordinator, Response> {
    attention.cloned().ok_or_else(|| {
        error_value(
            request,
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "notifications_not_configured",
                "the daemon is not configured for notification debounce".to_owned(),
                None,
            ),
        )
    })
}

/// Run a blocking notification operation off the async runtime.
async fn run_notification_blocking<T, F>(request: &Request, op: F) -> Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "notification_task_panicked",
        "notification operation task panicked",
        None,
    )
    .await
}

/// `notification.create`: create or dedupe a durable notification record.
///
/// Session-scoped attention and turn-completion creates are routed to the
/// debounce coordinator: the id is minted and `created: true` is returned, but
/// the record is held pending and only becomes visible in `notification.list`
/// after the debounce window (or never, if the session resumes first). Every
/// other create persists immediately.
pub(super) async fn handle_notification_create(
    request: &Request,
    notifications: Option<&NotificationService>,
    attention: Option<&AttentionCoordinator>,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<NotificationCreateParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    let params = enrich_notification_session_context(params, sessions).await;

    if is_debounced_create(params.kind, params.dedupe_key.as_deref()) {
        let attention = match require_attention(request, attention) {
            Ok(attention) => attention,
            Err(resp) => return resp,
        };
        return run_notification_blocking(request, move || {
            // Preparing the record does no store I/O; the coordinator commits it
            // after the debounce window. The response still returns the minted
            // record with `created: true`.
            let record = notifications
                .prepare_deferred(params)
                .map_err(|err| err.to_protocol_error())?;
            attention.defer(record.clone());
            Ok(NotificationCreateResult {
                created: true,
                record,
            })
        })
        .await;
    }

    run_notification_blocking(request, move || {
        notifications
            .create(params)
            .map_err(|err| err.to_protocol_error())
    })
    .await
}

/// Enrich producer params with live session context without making a missing
/// session reference fatal.
async fn enrich_notification_session_context(
    mut params: NotificationCreateParams,
    sessions: &SessionRegistry,
) -> NotificationCreateParams {
    let Some(session_id) = params.session_id.as_ref() else {
        return params;
    };
    let Ok(session) = sessions.inspect(session_id).await else {
        return params;
    };
    if session.state.is_terminal() {
        return params;
    }
    if params.agent_kind.is_none() {
        params.agent_kind = session.active_agent_base.or(Some(session.agent_base));
    }
    if params.project_id.is_none() {
        params.project_id = session.project_id;
    }
    params
}

/// `notification.list`: list durable notification records.
pub(super) async fn handle_notification_list(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Response {
    let params = match parse_optional_params::<NotificationListParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    run_notification_blocking(request, move || {
        notifications
            .list(params)
            .map_err(|err| err.to_protocol_error())
    })
    .await
}

/// `notification.update`: update notification lifecycle status.
pub(super) async fn handle_notification_update(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Response {
    let params = match parse_params::<NotificationUpdateParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    run_notification_blocking(request, move || {
        notifications
            .update(params)
            .map_err(|err| err.to_protocol_error())
    })
    .await
}

/// `notification.delete`: logically delete a notification.
pub(super) async fn handle_notification_delete(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Response {
    let params = match parse_params::<NotificationDeleteParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    run_notification_blocking(request, move || {
        notifications
            .delete(params)
            .map_err(|err| err.to_protocol_error())
    })
    .await
}

/// `notification.policy.get`: return the current notification policy.
pub(super) fn handle_notification_policy_get(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Response {
    if !request.params().is_null() {
        return error_value(
            request,
            ProtocolError::bad_request("notification.policy.get does not accept params"),
        );
    }
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    ok_value(
        request,
        &NotificationPolicyResult {
            policy: notifications.policy(),
        },
    )
}

/// `notification.policy.set`: persist a replacement notification policy.
pub(super) async fn handle_notification_policy_set(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Response {
    let params = match parse_params::<NotificationPolicyParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    let policy = params.policy;
    run_notification_blocking(request, move || {
        notifications
            .set_policy(policy.clone())
            .map_err(|err| err.to_protocol_error())?;
        Ok(NotificationPolicyResult { policy })
    })
    .await
}

/// `notification.retention.prune`: delete records selected by retention params.
pub(super) async fn handle_notification_retention_prune(
    request: &Request,
    notifications: Option<&NotificationService>,
) -> Response {
    let params = match parse_optional_params::<NotificationRetentionParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let notifications = match require_notifications(request, notifications) {
        Ok(notifications) => notifications,
        Err(resp) => return resp,
    };
    run_notification_blocking(request, move || {
        notifications
            .prune_retention(&params)
            .map_err(|err| err.to_protocol_error())
    })
    .await
}
