//! Approval notices from a session's terminal as attention.

// Rust guideline compliant 2026-10-12

use protocol::{
    NotificationCreateParams, NotificationKind, NotificationSeverity, NotificationSource, SessionId,
};
use tracing::warn;

use super::{
    attention_dedupe_key, AttentionCoordinator, DeferredNotification, NotificationService,
};

/// Provider namespace of the records a terminal notice produces.
const NOTICE_PROVIDER: &str = "pohunek";

/// Event name recorded as the source of a terminal notice.
const NOTICE_EVENT: &str = "terminal_notice";

/// Where an approval notice (`OSC 9`, `OSC 777 notify`, `OSC 99`) becomes an
/// `approval_required` record.
///
/// The record carries the shared attention dedupe key, so a blocked state the
/// projector derives, or a hook report, for the same moment merges into one
/// record whichever arrives first.
#[derive(Debug, Clone)]
pub struct ApprovalNoticeSink {
    notifications: NotificationService,
    attention: AttentionCoordinator,
}

impl ApprovalNoticeSink {
    /// A sink that creates records through `notifications` and holds them in
    /// the debounce window of `attention`.
    #[must_use]
    pub fn new(notifications: NotificationService, attention: AttentionCoordinator) -> Self {
        Self {
            notifications,
            attention,
        }
    }

    /// Holds an `approval_required` record of `session_id` in the debounce
    /// window. Preparation does no store I/O.
    pub(crate) fn approval_required(&self, session_id: &SessionId) {
        let params = NotificationCreateParams {
            source: NotificationSource {
                provider: NOTICE_PROVIDER.to_owned(),
                provider_event: NOTICE_EVENT.to_owned(),
                host_local_source_id: format!("notice:{}", session_id.0),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::ActionRequired,
            title: "Approval requested".to_owned(),
            body: format!("Session {} requested an approval.", session_id.0),
            metadata: std::collections::BTreeMap::from([(
                "reason".to_owned(),
                "terminal_notice".to_owned(),
            )]),
            session_id: Some(session_id.clone()),
            agent_kind: None,
            source_id: None,
            dedupe_key: Some(attention_dedupe_key(session_id)),
            project_id: None,
            retryable_admission: false,
        };
        match self.notifications.prepare_deferred(params) {
            Ok(deferred) => self.attention.defer(DeferredNotification {
                signal: true,
                ..deferred
            }),
            Err(error) => warn!(
                session_id = %session_id.0,
                %error,
                "failed to prepare the approval notice for debounce"
            ),
        }
    }
}
