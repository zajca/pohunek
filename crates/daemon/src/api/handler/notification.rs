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
use crate::notifications::{
    is_debounced_create, is_hook_provider, stored_provider, AttentionCoordinator,
    NotificationService,
};
use crate::session::{HookAdmissionError, SessionRegistry};

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
/// after the debounce window (or never, if the session resumes first). A
/// create that is an admitted provider hook claim for a session the registry
/// holds is also a turn signal: the coordinator records the matching turn
/// event, including for a kind the policy disables, which still answers
/// `notification_kind_disabled`. Every other create persists immediately.
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
    let hook_claim = match admit_hook_notification(request, &params, sessions).await {
        Ok(admitted) => admitted,
        Err(response) => return response,
    };
    // The daemon alone says which turn a report ends, so a producer's turn
    // report is attached here, never by its sender: to the turn the session's
    // current agent can still end, or to none, which leaves it without a
    // stamp.
    let turn = match (hook_claim, params.session_id.as_ref(), params.kind) {
        (true, Some(session_id), protocol::NotificationKind::TurnCompleted) => {
            sessions.attach_turn_report(session_id).await
        }
        _ => None,
    };
    // A report that attaches to no turn is still a turn signal for a held
    // session, as it is for the daemon's own blind spots (a turn the detector
    // did not see); `session.wait_turn` is at-least-once for such turns.
    let turn_signal = match (hook_claim, params.session_id.as_ref()) {
        (true, Some(session_id)) => sessions.holds(session_id).await,
        _ => false,
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
            // record with `created: true`. A turn signal whose kind the policy
            // disables still reaches the coordinator for `session.wait_turn`,
            // and the caller gets the policy refusal an immediate create reports.
            let mut deferred = notifications
                .prepare_deferred(params)
                .map_err(|err| err.to_protocol_error())?;
            deferred.signal = turn_signal;
            deferred.record.hook_admitted = hook_claim;
            if let Some(turn) = turn {
                deferred = deferred.for_turn(turn);
            }
            let refusal = deferred.policy_refusal();
            let record = deferred.record.clone();
            if deferred.inbox || deferred.signal {
                attention.defer(deferred);
            }
            if let Some(refusal) = refusal {
                return Err(refusal.to_protocol_error());
            }
            Ok(NotificationCreateResult {
                created: true,
                record,
            })
        })
        .await;
    }

    run_notification_blocking(request, move || {
        let created = if hook_claim {
            notifications.create_hook_claim(params)
        } else {
            notifications.create(params)
        };
        created.map_err(|err| err.to_protocol_error())
    })
    .await
}

/// Refuses a notification a provider hook reports for a session whose runtime
/// hook schema does not admit it.
///
/// A create that names a session and a provider with a hook schema is a hook
/// claim, so the public socket enforces the same admission as the worker
/// socket and a refusal there cannot be bypassed by retrying here. Creates
/// from other producers are hook claims only as the runtime of their own
/// session ([`admit_own_runtime_claim`]), and creates without a session are
/// not hook claims.
/// A claim for a session the registry neither holds nor is launching is
/// refused too, so no provider-ranked record exists for an id the daemon
/// cannot check.
/// The claim is judged by the provider name the record would be stored and
/// ranked under, and a hook claim must report that name verbatim: a spelling
/// that only normalizes to a hook provider (surrounding whitespace, control
/// characters) is refused, since shipped reporters send the canonical name.
/// Returns whether the create is an admitted hook claim.
async fn admit_hook_notification(
    request: &Request,
    params: &NotificationCreateParams,
    sessions: &SessionRegistry,
) -> Result<bool, Response> {
    let Some(session_id) = params.session_id.as_ref() else {
        return Ok(false);
    };
    let provider = stored_provider(&params.source.provider);
    if !is_hook_provider(&provider) {
        return admit_own_runtime_claim(request, params, &provider, session_id, sessions).await;
    }
    let refuse = |msg: &str| {
        error_value(
            request,
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "hook_not_admitted",
                msg.to_owned(),
                None,
            ),
        )
    };
    if provider != params.source.provider {
        return Err(refuse(
            "a hook report must name its provider exactly as the hook schema lists it",
        ));
    }
    sessions
        .admit_held_session_hook(
            session_id,
            pohunek_worker_protocol::HookAction::Notification,
            &provider,
            "hook notification",
            params.retryable_admission,
        )
        .await
        .map(|()| true)
        .map_err(|refusal| match refusal {
            // Only a report that opted in learns that a retry can succeed.
            HookAdmissionError::SchemaUnavailable { .. } if params.retryable_admission => {
                error_value(request, refusal.retryable_error())
            }
            HookAdmissionError::UnknownSession => {
                refuse("the daemon neither holds nor is launching a session with this id")
            }
            _ => refuse("the session runtime does not admit this hook report"),
        })
}

/// Judges a create whose provider is no listed hook provider: it is a hook
/// claim exactly when it names the runtime of a session the registry holds, by
/// that runtime's id spelled verbatim, and the runtime's hook schema admits it.
/// An observed external session, a launching session whose entry is not
/// installed, or a session removed since the lookup is not held, so it is no
/// claim.
///
/// Any other outcome is not a refusal: the producer name is free for scripts,
/// so the create is stored as a plain report without the hook claim's rank
/// (the WARN of a schema refusal still logs). Only a report that opted into the
/// retryable answer learns that the runtime's worker could not report its
/// schema in time.
async fn admit_own_runtime_claim(
    request: &Request,
    params: &NotificationCreateParams,
    provider: &str,
    session_id: &protocol::SessionId,
    sessions: &SessionRegistry,
) -> Result<bool, Response> {
    if provider != params.source.provider {
        return Ok(false);
    }
    let Ok(session) = sessions.inspect(session_id).await else {
        return Ok(false);
    };
    if session.agent_base.as_wire() != provider {
        return Ok(false);
    }
    match sessions
        .admit_held_session_hook(
            session_id,
            pohunek_worker_protocol::HookAction::Notification,
            provider,
            "hook notification",
            params.retryable_admission,
        )
        .await
    {
        Ok(()) => Ok(true),
        Err(refusal @ HookAdmissionError::SchemaUnavailable { .. })
            if params.retryable_admission =>
        {
            Err(error_value(request, refusal.retryable_error()))
        }
        Err(_refusal) => Ok(false),
    }
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
///
/// A result that does not fit one control line answers
/// `notification_list_too_large`, so the caller pages with `limit` instead of
/// receiving a frame its reader refuses.
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
    super::util::run_blocking_bounded(
        request,
        move || {
            notifications
                .list(params)
                .map_err(|err| err.to_protocol_error())
        },
        "notification_task_panicked",
        "notification operation task panicked",
        ProtocolError::notification_list_too_large(),
    )
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
    if let Err(error) = notifications.ensure_available() {
        return error_value(request, error.to_protocol_error());
    }
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
    let mut policy = params.policy;
    crate::notifications::complete_kind_flags(&mut policy);
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
