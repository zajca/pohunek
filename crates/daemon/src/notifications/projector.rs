//! Derived notifications from session events.

// Rust guideline compliant 2026-06-26

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use protocol::{
    event, AgentActivity, AgentKind, Event, NotificationCreateParams, NotificationKind,
    NotificationSeverity, NotificationSource, SessionId, SessionInfo, SessionState,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::session::SessionRegistry;

use super::{policy_enables_kind, NotificationService};

/// Provider namespace used for daemon-derived notification records.
const PROJECTOR_PROVIDER: &str = "pohunek";

/// Source-id prefix for daemon-derived notification records.
const PROJECTOR_SOURCE_ID_PREFIX: &str = "projector";

/// Dedupe-key prefix shared by projectors and provider hooks.
const ATTENTION_DEDUPE_KEY_PREFIX: &str = "attention";

/// Maximum time to wait for the projector task to flush buffered events.
///
/// Projector shutdown only drains local broadcast events and performs
/// owner-private JSONL appends through [`NotificationService`]. Five seconds
/// matches the daemon event-log shutdown budget and bounds a wedged filesystem.
const PROJECTOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Derives durable notifications from session lifecycle events.
#[derive(Debug)]
pub struct NotificationProjector {
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

impl NotificationProjector {
    /// Spawn a projector task on the session event broadcast.
    #[must_use]
    pub fn spawn(sessions: &SessionRegistry, notifications: NotificationService) -> Self {
        let shutdown = CancellationToken::new();
        let handle = spawn_projector_task(
            sessions.clone(),
            notifications,
            sessions.subscribe(),
            shutdown.clone(),
        );
        Self { shutdown, handle }
    }

    /// Stop the projector and drain buffered session events.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        match tokio::time::timeout(PROJECTOR_SHUTDOWN_TIMEOUT, self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "notification projector task failed during shutdown");
            }
            Err(_) => {
                warn!("notification projector did not finish within the shutdown timeout");
            }
        }
    }
}

fn spawn_projector_task(
    sessions: SessionRegistry,
    notifications: NotificationService,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = ProjectorState::default();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    drain_buffered_events(&sessions, &notifications, &mut events, &mut state).await;
                    break;
                }
                received = events.recv() => match received {
                    Ok(event) => state.handle_event_blocking(&notifications, &event).await,
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        warn!(
                            dropped,
                            "notification projector lagged; re-reading current session state"
                        );
                        resync_from_registry(&sessions, &notifications, &mut state).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

async fn drain_buffered_events(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    events: &mut broadcast::Receiver<Event>,
    state: &mut ProjectorState,
) {
    loop {
        match events.try_recv() {
            Ok(event) => state.handle_event_blocking(notifications, &event).await,
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                warn!(
                    dropped,
                    "notification projector lagged during shutdown; re-reading current session state"
                );
                resync_from_registry(sessions, notifications, state).await;
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
}

async fn resync_from_registry(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    state: &mut ProjectorState,
) {
    for session in sessions.list().await {
        state
            .handle_session_snapshot_blocking(notifications, &session)
            .await;
    }
}

/// Return the shared session attention dedupe key.
///
/// Scheme: `attention:<session_id>`. The key is source-independent so daemon
/// projectors and provider hooks can refer to the same waiting-for-input
/// condition without sharing producer-specific source ids.
#[must_use]
pub fn attention_dedupe_key(session_id: &SessionId) -> String {
    format!("{ATTENTION_DEDUPE_KEY_PREFIX}:{}", session_id.0)
}

/// Acknowledge lingering attention notifications for a resumed session.
///
/// Best-effort: a store failure is logged and swallowed so the projector event
/// loop keeps consuming session events instead of terminating on I/O errors.
fn resolve_session_attention(notifications: &NotificationService, session_id: &SessionId) {
    let dedupe_key = attention_dedupe_key(session_id);
    match notifications.resolve_attention(&dedupe_key) {
        Ok(resolved) if !resolved.is_empty() => {
            tracing::debug!(
                session = %session_id.0,
                resolved = resolved.len(),
                "acknowledged attention notifications after session resumed"
            );
        }
        Ok(_) => {}
        Err(error) => {
            warn!(
                session = %session_id.0,
                error = %error,
                "failed to resolve attention notifications after session resumed"
            );
        }
    }
}

/// Return a deterministic projector source id.
///
/// Scheme: `projector:<session_id>:<kind>:<transition_epoch>`. The transition
/// epoch is local to one daemon projector instance and increments only when the
/// session enters a projected state for that notification kind.
#[must_use]
fn projector_source_id(
    session_id: &SessionId,
    kind: NotificationKind,
    transition_epoch: u64,
) -> String {
    format!(
        "{PROJECTOR_SOURCE_ID_PREFIX}:{}:{}:{transition_epoch}",
        session_id.0,
        kind.as_str()
    )
}

#[derive(Debug, Default)]
struct ProjectorState {
    activity_by_session: HashMap<SessionId, AgentActivity>,
    lifecycle_by_session: HashMap<SessionId, SessionState>,
    transition_epochs: HashMap<(SessionId, NotificationKind), u64>,
}

impl ProjectorState {
    #[cfg(test)]
    fn handle_event(&mut self, notifications: &NotificationService, event: &Event) {
        for pending in self.pending_event(notifications, event) {
            create_pending_notification(notifications, pending);
        }
    }

    async fn handle_event_blocking(&mut self, notifications: &NotificationService, event: &Event) {
        for pending in self.pending_event(notifications, event) {
            create_pending_notification_blocking(notifications.clone(), pending).await;
        }
    }

    async fn handle_session_snapshot_blocking(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) {
        for pending in self.pending_session_snapshot(notifications, session) {
            create_pending_notification_blocking(notifications.clone(), pending).await;
        }
    }

    fn pending_event(
        &mut self,
        notifications: &NotificationService,
        event: &Event,
    ) -> Vec<PendingNotification> {
        match event.event.as_str() {
            event::AGENT_STATE => {
                if let Some(payload) = parse_projector_payload::<AgentStatePayload>(event) {
                    return self
                        .pending_activity(notifications, &payload.session_id, payload.activity)
                        .into_iter()
                        .collect();
                }
            }
            event::SESSION_UPDATED => {
                if let Some(payload) = parse_projector_payload::<SessionPayload>(event) {
                    return self
                        .pending_session_updated(notifications, &payload.session)
                        .into_iter()
                        .collect();
                }
            }
            event::SESSION_STOPPED => {
                if let Some(payload) = parse_projector_payload::<SessionPayload>(event) {
                    self.handle_explicit_stop(&payload.session);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn pending_session_snapshot(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) -> Vec<PendingNotification> {
        let mut pending = Vec::new();
        if let Some(activity) = session.activity {
            pending.extend(self.pending_activity(notifications, &session.id, activity));
        } else {
            self.activity_by_session.remove(&session.id);
        }
        if session.state == SessionState::Stopped {
            self.handle_explicit_stop(session);
        } else {
            pending.extend(self.pending_session_updated(notifications, session));
        }
        pending
    }

    fn pending_activity(
        &mut self,
        notifications: &NotificationService,
        session_id: &SessionId,
        activity: AgentActivity,
    ) -> Option<PendingNotification> {
        let previous = self
            .activity_by_session
            .insert(session_id.clone(), activity);
        // A session that resumes active work no longer needs owner attention, so
        // acknowledge any lingering attention notifications sharing its dedupe
        // key. Only the transition edge into `Working` triggers the resolve so
        // repeated working events do not rescan the store. Do not gate on the
        // previous state being `Blocked`: provider hooks create attention
        // notifications without the daemon observing a blocked activity edge.
        if activity == AgentActivity::Working && previous != Some(AgentActivity::Working) {
            resolve_session_attention(notifications, session_id);
        }
        if activity != AgentActivity::Blocked || previous == Some(AgentActivity::Blocked) {
            return None;
        }
        self.prepare_derived(
            notifications,
            DerivedNotification {
                session_id: session_id.clone(),
                kind: NotificationKind::AgentBlocked,
                severity: NotificationSeverity::ActionRequired,
                source_event: event::AGENT_STATE,
                title: "Agent needs attention".to_owned(),
                body: format!("Session {} is waiting for owner attention.", session_id.0),
                metadata: BTreeMap::from([("reason".to_owned(), "blocked".to_owned())]),
                agent_kind: None,
                project_id: None,
                dedupe_key: Some(attention_dedupe_key(session_id)),
            },
        )
    }

    fn pending_session_updated(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) -> Option<PendingNotification> {
        let previous = self
            .lifecycle_by_session
            .insert(session.id.clone(), session.state);
        match session.state {
            SessionState::Failed if previous != Some(SessionState::Failed) => {
                self.activity_by_session.remove(&session.id);
                self.pending_failed_notification(notifications, session)
            }
            SessionState::Done if previous != Some(SessionState::Done) => {
                self.activity_by_session.remove(&session.id);
                self.pending_finished_notification(notifications, session)
            }
            SessionState::Stopped => {
                self.handle_explicit_stop(session);
                None
            }
            SessionState::Starting
            | SessionState::Running
            | SessionState::Failed
            | SessionState::Done => None,
        }
    }

    fn handle_explicit_stop(&mut self, session: &SessionInfo) {
        self.lifecycle_by_session
            .insert(session.id.clone(), SessionState::Stopped);
        self.activity_by_session.remove(&session.id);
    }

    fn pending_failed_notification(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) -> Option<PendingNotification> {
        let (body, reason) = if let Some(exit_code) = session.exit_code {
            (
                format!(
                    "Session {} failed with exit code {exit_code}.",
                    session.id.0
                ),
                format!("exit_code={exit_code}"),
            )
        } else {
            (
                format!("Session {} failed.", session.id.0),
                "failed".to_owned(),
            )
        };
        self.prepare_derived(
            notifications,
            DerivedNotification {
                session_id: session.id.clone(),
                kind: NotificationKind::Error,
                severity: NotificationSeverity::Error,
                source_event: event::SESSION_UPDATED,
                title: "Session failed".to_owned(),
                body,
                metadata: BTreeMap::from([("reason".to_owned(), reason)]),
                agent_kind: Some(session_agent_kind(session)),
                project_id: session.project_id.clone(),
                dedupe_key: None,
            },
        )
    }

    fn pending_finished_notification(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) -> Option<PendingNotification> {
        self.prepare_derived(
            notifications,
            DerivedNotification {
                session_id: session.id.clone(),
                kind: NotificationKind::SessionFinished,
                severity: NotificationSeverity::Success,
                source_event: event::SESSION_UPDATED,
                title: "Session finished".to_owned(),
                body: format!("Session {} finished successfully.", session.id.0),
                metadata: BTreeMap::from([("summary".to_owned(), "session_finished".to_owned())]),
                agent_kind: Some(session_agent_kind(session)),
                project_id: session.project_id.clone(),
                dedupe_key: None,
            },
        )
    }

    fn prepare_derived(
        &mut self,
        notifications: &NotificationService,
        derived: DerivedNotification,
    ) -> Option<PendingNotification> {
        if !policy_enables_kind(&notifications.policy(), PROJECTOR_PROVIDER, derived.kind) {
            return None;
        }
        let transition_epoch = self.next_transition_epoch(&derived.session_id, derived.kind);
        let source_id = projector_source_id(&derived.session_id, derived.kind, transition_epoch);
        let params = NotificationCreateParams {
            source: NotificationSource {
                provider: PROJECTOR_PROVIDER.to_owned(),
                provider_event: derived.source_event.to_owned(),
                host_local_source_id: source_id.clone(),
            },
            kind: derived.kind,
            severity: derived.severity,
            title: derived.title,
            body: derived.body,
            metadata: derived.metadata,
            session_id: Some(derived.session_id.clone()),
            agent_kind: derived.agent_kind,
            source_id: Some(source_id),
            dedupe_key: derived.dedupe_key,
            project_id: derived.project_id,
        };
        Some(PendingNotification {
            session_id: derived.session_id,
            kind: derived.kind,
            params,
        })
    }

    fn next_transition_epoch(&mut self, session_id: &SessionId, kind: NotificationKind) -> u64 {
        let epoch = self
            .transition_epochs
            .entry((session_id.clone(), kind))
            .or_insert(0);
        *epoch += 1;
        *epoch
    }
}

#[derive(Debug)]
struct PendingNotification {
    session_id: SessionId,
    kind: NotificationKind,
    params: NotificationCreateParams,
}

#[cfg(test)]
fn create_pending_notification(notifications: &NotificationService, pending: PendingNotification) {
    if let Err(err) = notifications.create(pending.params) {
        warn!(
            error = %err,
            session_id = %pending.session_id.0,
            kind = pending.kind.as_str(),
            "failed to create derived notification"
        );
    }
}

async fn create_pending_notification_blocking(
    notifications: NotificationService,
    pending: PendingNotification,
) {
    let session_id = pending.session_id;
    let kind = pending.kind;
    let params = pending.params;
    // Await each blocking create before handling the next event. That keeps
    // derived notifications in session event order while moving sync JSONL
    // append/flush work off the Tokio runtime worker.
    match run_projector_blocking(move || notifications.create(params)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            warn!(
                error = %err,
                session_id = %session_id.0,
                kind = kind.as_str(),
                "failed to create derived notification"
            );
        }
        Err(err) => {
            warn!(
                error = %err,
                session_id = %session_id.0,
                kind = kind.as_str(),
                "notification projector blocking create task failed"
            );
        }
    }
}

async fn run_projector_blocking<F, T>(op: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(op).await
}

#[derive(Debug)]
struct DerivedNotification {
    session_id: SessionId,
    kind: NotificationKind,
    severity: NotificationSeverity,
    source_event: &'static str,
    title: String,
    body: String,
    metadata: BTreeMap<String, String>,
    agent_kind: Option<AgentKind>,
    project_id: Option<String>,
    dedupe_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentStatePayload {
    session_id: SessionId,
    activity: AgentActivity,
}

#[derive(Debug, Deserialize)]
struct SessionPayload {
    session: SessionInfo,
}

fn parse_projector_payload<T>(event: &Event) -> Option<T>
where
    T: DeserializeOwned,
{
    match serde_json::from_value(event.payload.clone()) {
        Ok(payload) => Some(payload),
        Err(err) => {
            warn!(
                event = %event.event,
                error = %err,
                "failed to parse session event for notification projector"
            );
            None
        }
    }
}

fn session_agent_kind(session: &SessionInfo) -> AgentKind {
    session.active_agent_base.unwrap_or(session.agent_base)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::{
        event, AgentActivity, AgentKind, Event, NotificationCreateParams, NotificationKind,
        NotificationKindPolicy, NotificationListParams, NotificationSeverity, NotificationSource,
        NotificationStatus, SessionId, SessionInfo, SessionState, StateSource,
    };
    use serde_json::json;

    use super::{
        attention_dedupe_key, projector_source_id, run_projector_blocking, ProjectorState,
    };
    use crate::notifications::{default_policy, NotificationService};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_data_dir(tag: &str) -> std::path::PathBuf {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pohunek-notification-projector-{tag}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    fn service(tag: &str) -> NotificationService {
        NotificationService::open(&temp_data_dir(tag)).expect("open notification service")
    }

    fn list(service: &NotificationService) -> Vec<protocol::NotificationRecord> {
        service
            .list(NotificationListParams::default())
            .expect("list notifications")
            .notifications
    }

    fn agent_state_event(session_id: &str, activity: AgentActivity) -> Event {
        Event::new(
            event::AGENT_STATE,
            json!({
                "session_id": session_id,
                "activity": activity,
                "source": StateSource::Report,
            }),
        )
    }

    fn session_event(event_name: &str, info: &SessionInfo) -> Event {
        Event::new(event_name, json!({ "session": info }))
    }

    fn session_info(session_id: &str, state: SessionState, exit_code: Option<i32>) -> SessionInfo {
        SessionInfo {
            id: SessionId(session_id.to_owned()),
            name: None,
            agent: "codex".to_owned(),
            agent_base: AgentKind::Codex,
            cwd: std::path::PathBuf::from("/workspace/project"),
            pid: 1234,
            cols: 120,
            rows: 40,
            state,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            project_id: Some("p-test".to_owned()),
            project_label: None,
            is_linked_worktree: Some(false),
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: "2026-07-03T10:00:00Z".to_owned(),
            updated_at: "2026-07-03T10:01:00Z".to_owned(),
            exit_code,
        }
    }

    fn enable_session_finished(service: &NotificationService) {
        let mut policy = default_policy();
        policy.enabled = NotificationKindPolicy {
            session_finished: true,
            ..policy.enabled
        };
        policy.codex = None;
        policy.claude = None;
        service.set_policy(policy).expect("set notification policy");
    }

    fn provider_approval_params(session_id: &str) -> NotificationCreateParams {
        let session_id = SessionId(session_id.to_owned());
        NotificationCreateParams {
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: "codex-permission-s-1".to_owned(),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::ActionRequired,
            title: "Approval required".to_owned(),
            body: "Codex needs approval.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: Some(session_id.clone()),
            agent_kind: Some(AgentKind::Codex),
            source_id: Some("codex:s-1:permission:1".to_owned()),
            dedupe_key: Some(attention_dedupe_key(&session_id)),
            project_id: Some("p-test".to_owned()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn projector_blocking_helper_runs_store_create_off_runtime_worker() {
        let service = service("blocking-helper");
        let params = provider_approval_params("s-blocking-helper");
        let runtime_thread = std::thread::current().id();

        let blocking_thread = run_projector_blocking(move || {
            service
                .create(params)
                .expect("blocking helper creates notification");
            std::thread::current().id()
        })
        .await
        .expect("blocking helper task joins");

        assert_ne!(
            blocking_thread, runtime_thread,
            "projector store writes must not run on the async runtime worker"
        );
    }

    #[test]
    fn blocked_transition_creates_agent_blocked_notification() {
        let service = service("blocked");
        let mut projector = ProjectorState::default();

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        let record = &notifications[0];
        assert_eq!(record.kind, NotificationKind::AgentBlocked);
        assert_eq!(record.severity, NotificationSeverity::ActionRequired);
        assert_eq!(record.status, NotificationStatus::Unread);
        assert_eq!(record.session_id, Some(SessionId("s-1".to_owned())));
        assert_eq!(record.source.provider, "pohunek");
        assert_eq!(record.source.provider_event, event::AGENT_STATE);
        assert_eq!(
            record.source_id.as_deref(),
            Some("projector:s-1:agent_blocked:1")
        );
        assert_eq!(record.dedupe_key.as_deref(), Some("attention:s-1"));
    }

    #[test]
    fn working_after_blocked_acknowledges_projector_notification() {
        let service = service("resolve-projector");
        let mut projector = ProjectorState::default();

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));
        let created = list(&service);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].kind, NotificationKind::AgentBlocked);
        assert_eq!(created[0].status, NotificationStatus::Unread);

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Working));

        let resolved = list(&service);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].status, NotificationStatus::Acknowledged);
        assert!(resolved[0].acked_at.is_some());
    }

    #[test]
    fn working_acknowledges_provider_hook_attention_notification() {
        let service = service("resolve-hook");
        let mut projector = ProjectorState::default();

        // A provider hook creates the attention notification directly, so the
        // projector never observes a blocked activity edge for this session.
        service
            .create(provider_approval_params("s-1"))
            .expect("create provider approval notification");
        let created = list(&service);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].kind, NotificationKind::ApprovalRequired);
        assert_eq!(created[0].status, NotificationStatus::Unread);

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Working));

        let resolved = list(&service);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].status, NotificationStatus::Acknowledged);
    }

    #[test]
    fn repeated_blocked_events_during_one_period_do_not_duplicate() {
        let service = service("blocked-dedupe");
        let mut projector = ProjectorState::default();

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));
        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));
        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));

        assert_eq!(list(&service).len(), 1);
    }

    #[test]
    fn provider_approval_suppresses_projector_blocked_for_same_attention_key() {
        let service = service("provider-suppresses-projector");
        let provider = service
            .create(provider_approval_params("s-1"))
            .expect("provider create");
        let mut projector = ProjectorState::default();

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].id, provider.record.id);
        assert_eq!(notifications[0].kind, NotificationKind::ApprovalRequired);
        assert_eq!(notifications[0].source.provider, "codex");
    }

    #[test]
    fn failed_session_update_creates_error_with_exit_code() {
        let service = service("failed");
        let mut projector = ProjectorState::default();

        projector.handle_event(
            &service,
            &session_event(
                event::SESSION_UPDATED,
                &session_info("s-1", SessionState::Failed, Some(42)),
            ),
        );

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        let record = &notifications[0];
        assert_eq!(record.kind, NotificationKind::Error);
        assert_eq!(record.severity, NotificationSeverity::Error);
        assert_eq!(record.source_id.as_deref(), Some("projector:s-1:error:1"));
        assert!(
            record.body.contains("exit code 42"),
            "body must include the safe exit code value: {}",
            record.body
        );
        assert_eq!(
            record.metadata.get("reason").map(String::as_str),
            Some("exit_code=42")
        );
    }

    #[test]
    fn done_session_update_creates_session_finished_when_policy_enabled() {
        let service = service("done-enabled");
        enable_session_finished(&service);
        let mut projector = ProjectorState::default();

        projector.handle_event(
            &service,
            &session_event(
                event::SESSION_UPDATED,
                &session_info("s-1", SessionState::Done, Some(0)),
            ),
        );

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].kind, NotificationKind::SessionFinished);
        assert_eq!(notifications[0].severity, NotificationSeverity::Success);
        assert_eq!(
            notifications[0].source_id.as_deref(),
            Some("projector:s-1:session_finished:1")
        );
    }

    #[test]
    fn stopped_session_events_do_not_create_error_notifications() {
        let service = service("stopped");
        let mut projector = ProjectorState::default();

        projector.handle_event(
            &service,
            &session_event(
                event::SESSION_STOPPED,
                &session_info("s-1", SessionState::Stopped, None),
            ),
        );
        projector.handle_event(
            &service,
            &session_event(
                event::SESSION_UPDATED,
                &session_info("s-2", SessionState::Stopped, None),
            ),
        );

        assert!(list(&service).is_empty());
    }

    #[test]
    fn disabled_policy_kinds_do_not_create_records() {
        let service = service("done-disabled");
        let mut projector = ProjectorState::default();

        projector.handle_event(
            &service,
            &session_event(
                event::SESSION_UPDATED,
                &session_info("s-1", SessionState::Done, Some(0)),
            ),
        );

        assert!(list(&service).is_empty());
    }

    #[test]
    fn source_ids_are_deterministic_per_kind_transition_epoch() {
        let service = service("source-id");
        let mut projector = ProjectorState::default();

        projector.handle_event(&service, &agent_state_event("s-1", AgentActivity::Blocked));

        let source_ids = list(&service)
            .into_iter()
            .map(|record| record.source_id.expect("projector source id"))
            .collect::<Vec<_>>();
        assert_eq!(source_ids, vec!["projector:s-1:agent_blocked:1".to_owned()]);
        assert_eq!(
            projector_source_id(&SessionId("s-2".to_owned()), NotificationKind::Error, 3),
            "projector:s-2:error:3"
        );
    }
}
