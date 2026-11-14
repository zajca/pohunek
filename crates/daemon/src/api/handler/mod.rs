//! Control-method dispatch.
//!
//! Parses a newline-delimited JSON request line into a [`protocol::Request`],
//! negotiates the protocol version, dispatches to the method handler, and
//! serializes a [`protocol::Response`] back to a single line.
//!
//! This module is the router and transport glue: [`dispatch_line`] frames one
//! request, [`handle_request`] fans the parsed request to the per-domain handler
//! modules ([`session`], [`project`], [`worktree`], [`host`], [`daemon`],
//! [`notification`], [`assistant`], [`integration`]), and the [`util`] module
//! holds the shared parse/serialize/blocking helpers those handlers reuse. A
//! `subscribe` request is dispatched specially so the caller can turn the
//! connection into a one-way event stream. Unknown methods get a typed
//! `method_not_found` error so older daemons degrade predictably as the CLI gains
//! methods.

mod assistant;
#[cfg(test)]
mod attention_isolation_tests;
mod command;
mod daemon;
mod governance;
mod host;
mod integration;
mod notification;
mod package;
mod project;
mod session;
#[cfg(test)]
#[cfg(test)]
mod turn_wait_tests;
mod util;
mod worktree;

use protocol::{
    compat, method, negotiate, ProtocolError, ProtocolVersion, Request, Response, PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, warn};

use pohunek_client::OverlayRegistry;

use super::audit;
use crate::discovery::DiscoveryCache;
use crate::governance::HostGovernanceService;
use crate::notifications::{AttentionCoordinator, NotificationService};
use crate::package_command::PackageCommandService;
use crate::session::{ControlPeer, SessionRegistry};

/// Static facts the daemon reports from `daemon.health`.
///
/// Cloned into each connection task. Cheap to clone (two short strings).
#[derive(Debug, Clone)]
pub struct HealthInfo {
    /// Daemon build version (e.g. crate version).
    pub daemon_version: String,
}

/// The transport a control connection arrived on.
///
/// Owner-only methods that extend the host's launch authority, such as the
/// `package.*` lifecycle, are served on the local socket only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlTransport {
    /// The local Unix control socket, and in-process dispatch.
    #[default]
    Local,
    /// A remote overlay TCP connection.
    Remote,
}

/// Shared daemon state available to every control connection.
#[derive(Debug, Clone)]
pub struct DaemonState {
    /// Static health metadata.
    pub health: HealthInfo,
    /// In-memory session registry.
    pub sessions: SessionRegistry,
    /// Daemon-owned stable host governance, shared by every owner transport.
    pub governance: Arc<HostGovernanceService>,
    /// Durable notification inbox, when configured by the daemon binary.
    pub notifications: Option<NotificationService>,
    /// Attention debounce coordinator, when notifications are configured.
    pub attention: Option<AttentionCoordinator>,
    /// TTL-cached `NetBird` host discovery, shared across connections.
    pub discovery: DiscoveryCache,
    /// Where the registry of `discovery` came from, which decides how severe a
    /// missing provider CLI is in the doctor.
    pub overlay_source: OverlaySource,
    /// The transport the connections served with this state arrive on.
    pub transport: ControlTransport,
    /// The kernel-attested peer of the one local connection this state serves.
    ///
    /// Set per accepted Unix connection. `None` on every other state, so a
    /// caller without an attested peer is refused where one is required.
    pub control_peer: Option<Arc<ControlPeer>>,
    /// The overlay address of the one remote connection this state serves.
    ///
    /// Set per accepted remote connection; `None` on every other state.
    pub remote_peer: Option<SocketAddr>,
    /// Runs package commands. States that serve one daemon share one service,
    /// so its limits hold across transports.
    pub package_commands: PackageCommandService,
}

/// Where a daemon's overlay registry came from.
///
/// The doctor treats a missing provider CLI as a failure for an overlay the
/// owner listed in `overlays.toml` and as a warning for the `NetBird` default of
/// an absent file, so every state is built with the source of its registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySource {
    /// The default of an absent `overlays.toml`.
    Default,
    /// The providers listed in `overlays.toml`.
    File,
}

impl OverlaySource {
    /// The source of a loaded overlay configuration.
    #[must_use]
    pub fn of(config: &pohunek_client::OverlayConfig) -> Self {
        if config.from_file() {
            Self::File
        } else {
            Self::Default
        }
    }

    fn is_file(self) -> bool {
        self == Self::File
    }
}

impl DaemonState {
    /// Construct shared daemon state with a validated overlay registry.
    #[must_use]
    pub fn new(
        health: HealthInfo,
        sessions: SessionRegistry,
        governance: Arc<HostGovernanceService>,
        registry: OverlayRegistry,
        overlay_source: OverlaySource,
    ) -> Self {
        Self::new_with_discovery(
            health,
            sessions,
            governance,
            DiscoveryCache::new(registry),
            overlay_source,
        )
    }

    /// Construct shared daemon state with an existing discovery cache.
    #[must_use]
    pub fn new_with_discovery(
        health: HealthInfo,
        sessions: SessionRegistry,
        governance: Arc<HostGovernanceService>,
        discovery: DiscoveryCache,
        overlay_source: OverlaySource,
    ) -> Self {
        let package_commands = PackageCommandService::new(sessions.package_command_host());
        Self {
            health,
            sessions,
            governance,
            notifications: None,
            attention: None,
            discovery,
            overlay_source,
            transport: ControlTransport::default(),
            control_peer: None,
            remote_peer: None,
            package_commands,
        }
    }

    /// Serve package commands with `service`, the one the daemon's other
    /// states use, so the run limits are shared.
    #[must_use]
    pub fn with_package_commands(mut self, service: PackageCommandService) -> Self {
        self.package_commands = service;
        self
    }

    /// Mark the connections served with this state as arriving on `transport`.
    ///
    /// A listener's state carries no connection peer; the local accept loop
    /// attaches one to each accepted connection.
    #[must_use]
    pub fn with_transport(mut self, transport: ControlTransport) -> Self {
        self.transport = transport;
        self.control_peer = None;
        self.remote_peer = None;
        self
    }

    /// Attach the overlay address of one accepted remote connection.
    #[must_use]
    pub fn with_remote_peer(mut self, peer: SocketAddr) -> Self {
        self.remote_peer = Some(peer);
        self
    }

    /// Attach the kernel-attested peer of one accepted connection.
    #[must_use]
    pub fn with_control_peer(mut self, peer: Option<Arc<ControlPeer>>) -> Self {
        self.control_peer = peer;
        self
    }

    /// Attach the durable notification service to shared daemon state.
    #[must_use]
    pub fn with_notifications(mut self, notifications: NotificationService) -> Self {
        self.notifications = Some(notifications);
        self
    }

    /// Attach the session notification debounce coordinator to shared daemon state.
    #[must_use]
    pub fn with_attention_coordinator(mut self, attention: AttentionCoordinator) -> Self {
        self.attention = Some(attention);
        self
    }
}

impl HealthInfo {
    /// Construct health info from a daemon version string.
    #[must_use]
    pub fn new(daemon_version: impl Into<String>) -> Self {
        Self {
            daemon_version: daemon_version.into(),
        }
    }
}

/// Outcome of dispatching one request line.
///
/// Most requests are one-shot (`Reply`), but a `subscribe` request asks the
/// connection to become a one-way event stream after an OK ack (`Subscribe`).
#[derive(Debug)]
pub(crate) enum Dispatch {
    /// One-shot: send this response line, then keep reading requests.
    Reply(String),
    /// The client asked to subscribe; send this OK ack line, then the caller
    /// streams session events on this connection until the client disconnects.
    Subscribe(String, protocol::ProtocolVersion),
    /// The client sent an attach prelude; the caller switches to raw PTY bytes.
    Attach(String),
}

/// Parse one request line and decide how the connection should proceed.
///
/// Never panics and never returns an error: malformed input and version
/// mismatches are turned into typed error responses (`Reply`) so the connection
/// can stay open for the next request. A valid `subscribe` request yields
/// `Subscribe` with the OK ack line for the caller to write before streaming.
///
/// `connection_version` is the version already frozen for this connection. A
/// line that cannot be parsed has no range of its own, so its error is stamped
/// with that version; before the first successful request nothing is negotiated
/// yet and the daemon's current version is the only honest stamp.
pub(crate) async fn dispatch_line(
    line: &str,
    state: &DaemonState,
    connection_version: Option<ProtocolVersion>,
) -> Dispatch {
    let error_version = connection_version.unwrap_or(PROTOCOL_VERSION);
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Tolerate blank keep-alive lines; reply with a framing error tied to a
        // synthetic id so the client still gets a parseable line.
        let resp = Response::err(
            error_version,
            "invalid-request",
            ProtocolError::bad_request("empty request line"),
        )
        .expect("synthetic response id is valid");
        return Dispatch::Reply(serialize_response(&resp));
    }

    if let Some(stream_id) = util::parse_attach_prelude(trimmed) {
        return Dispatch::Attach(stream_id);
    }

    let request: Request = match serde_json::from_str(trimmed) {
        Ok(req) => req,
        Err(err) => {
            warn!(error = %err, "failed to parse control request");
            // We cannot recover the request id from unparseable JSON; use empty.
            let resp = Response::err(
                error_version,
                "invalid-request",
                ProtocolError::bad_request(format!("invalid request JSON: {err}")),
            )
            .expect("synthetic response id is valid");
            return Dispatch::Reply(serialize_response(&resp));
        }
    };

    // A detach request names only the stream; its session is read before the
    // request ends the stream.
    let detached_session = if request.method() == method::SESSION_DETACH {
        match serde_json::from_value::<protocol::SessionDetachParams>(request.params().clone()) {
            Ok(params) => state.sessions.attach_session_id(&params.stream_id).await,
            Err(_) => None,
        }
    } else {
        None
    };
    let resp = handle_request(&request, state).await;
    audit::record_request(
        &request,
        &resp,
        state,
        detached_session.as_ref().map(|id| id.0.as_str()),
    );
    if request.method() == method::SUBSCRIBE && resp.is_ok() {
        let version = resp.version();
        return Dispatch::Subscribe(serialize_response(&resp), version);
    }
    Dispatch::Reply(serialize_response(&resp))
}

/// Dispatch a parsed request to its method handler.
///
/// Negotiates the protocol version first: a request outside the window is
/// refused with `daemon/version_mismatch` before any method runs, and the
/// refusal carries the current version. The window holds the current version
/// alone, so a handler sees the request as sent and its response goes back
/// untranslated; [`protocol::compat`] would translate a negotiated version
/// other than the current one, which no peer can select.
///
/// Exposed within the crate (and re-exported) so integration tests can exercise
/// dispatch without a live socket.
#[must_use]
pub async fn handle_request(request: &Request, state: &DaemonState) -> Response {
    debug!(id = %request.id(), method = %request.method(), "control request");

    // Version negotiation first: an incompatible client gets a typed error
    // rather than a confusingly-shaped success.
    let selected_version = match negotiate(request.version_range(), SUPPORTED_PROTOCOL_VERSIONS) {
        Ok(version) => version,
        Err(err) => {
            return Response::err(PROTOCOL_VERSION, request.id(), err)
                .expect("deserialized request ids satisfy response validation");
        }
    };
    if selected_version == PROTOCOL_VERSION {
        return handle_negotiated(request, state, selected_version).await;
    }

    let adapted = match compat::upgrade_request(request.clone(), selected_version) {
        Ok(adapted) => adapted,
        // A method added after the negotiated version is unknown to it.
        Err(compat::CompatError::MethodNotDefined { method }) => {
            return Response::err(
                selected_version,
                request.id(),
                ProtocolError::method_not_found(&method),
            )
            .expect("deserialized request ids satisfy response validation");
        }
        Err(error) => {
            warn!(
                method = %request.method(),
                protocol_version = selected_version.get(),
                %error,
                "request does not match the negotiated protocol version"
            );
            return Response::err(
                selected_version,
                request.id(),
                ProtocolError::bad_request(format!(
                    "request is not valid for protocol version {selected_version}: {error}"
                )),
            )
            .expect("deserialized request ids satisfy response validation");
        }
    };
    let response = handle_negotiated(&adapted, state, selected_version).await;
    match compat::downgrade_response(response, request.method()) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                method = %request.method(),
                protocol_version = selected_version.get(),
                %error,
                "response cannot be expressed in the negotiated protocol version"
            );
            Response::err(
                selected_version,
                request.id(),
                ProtocolError::version_adapter_failed(selected_version),
            )
            .expect("deserialized request ids satisfy response validation")
        }
    }
}

/// Dispatch a request whose parameters already have the current shape.
#[expect(
    clippy::too_many_lines,
    reason = "the public method table stays centralized so version and origin guards apply uniformly"
)]
async fn handle_negotiated(
    request: &Request,
    state: &DaemonState,
    selected_version: protocol::ProtocolVersion,
) -> Response {
    if let Some(target) = mutation_target(request) {
        if state
            .sessions
            .is_origin_session(
                request.origin_session_id(),
                request.origin_daemon_id(),
                target,
            )
            .await
        {
            return Response::err(
                selected_version,
                request.id(),
                ProtocolError::plugin_self_target_denied(),
            )
            .expect("deserialized request ids satisfy response validation");
        }
    }

    // Stop and remove release what a session owns and need no resolvable
    // runtime; the registry refuses an external session itself.
    let releases_only = matches!(
        request.method(),
        method::SESSION_STOP
            | method::SESSION_REMOVE
            | method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED
    );
    if let Some(target) = mutation_target(request).filter(|_| !releases_only) {
        if let Err(error) = state.sessions.ensure_known_agent(target).await {
            return Response::err(selected_version, request.id(), error)
                .expect("deserialized request ids satisfy response validation");
        }
    }

    match request.method() {
        method::DAEMON_HEALTH => daemon::handle_health(request, &state.health),
        method::SUBSCRIBE => Response::ok(
            selected_version,
            request.id(),
            json!({ "subscribed": true }),
        )
        .expect("deserialized request ids satisfy response validation"),
        method::SESSION_NEW => session::handle_session_new(request, &state.sessions).await,
        method::SESSION_LIST => session::handle_session_list(request, &state.sessions).await,
        method::SESSION_INSPECT => session::handle_session_inspect(request, &state.sessions).await,
        method::SESSION_STOP => session::handle_session_stop(request, &state.sessions).await,
        method::SESSION_RESUME => session::handle_session_resume(request, state).await,
        method::SESSION_REMOVE => session::handle_session_remove(request, &state.sessions).await,
        method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED => {
            session::handle_session_remove_accepting_unconfirmed(request, &state.sessions).await
        }
        method::SESSION_ATTACH => session::handle_session_attach(request, &state.sessions).await,
        method::SESSION_DETACH => session::handle_session_detach(request, &state.sessions).await,
        method::SESSION_RESIZE => session::handle_session_resize(request, &state.sessions).await,
        method::SESSION_SET_METADATA => {
            session::handle_session_set_metadata(request, &state.sessions).await
        }
        method::SESSION_RENAME => session::handle_session_rename(request, &state.sessions).await,
        method::SESSION_DIFF => session::handle_session_diff(request, &state.sessions).await,
        method::SESSION_RETENTION_SWEEP => {
            session::handle_session_retention_sweep(request, &state.sessions).await
        }
        method::SESSION_INPUT => session::handle_session_input(request, &state.sessions).await,
        method::SESSION_SCREEN => session::handle_session_screen(request, &state.sessions).await,
        method::SESSION_DETECTION => {
            session::handle_session_detection(request, &state.sessions).await
        }
        method::SESSION_OUTPUT => session::handle_session_output(request, &state.sessions).await,
        method::SESSION_READ => session::handle_session_read(request, &state.sessions).await,
        method::SESSION_WAIT => session::handle_session_wait(request, &state.sessions).await,
        method::SESSION_WAIT_TURN => {
            session::handle_session_wait_turn(request, &state.sessions).await
        }
        method::SESSION_REPORT_NATIVE_ID => {
            session::handle_session_report_native_id(
                request,
                &state.sessions,
                state.control_peer.as_deref(),
            )
            .await
        }
        method::SESSION_REPORT_AGENT => {
            session::handle_session_report_agent(request, &state.sessions).await
        }
        method::SESSION_RELEASE_AGENT => {
            session::handle_session_release_agent(request, &state.sessions).await
        }
        method::DAEMON_DOCTOR => {
            daemon::handle_daemon_doctor(
                request,
                &state.governance,
                &state.sessions,
                state.discovery.registry(),
                state.overlay_source.is_file(),
                state.transport,
            )
            .await
        }
        method::ASSISTANT_MATERIALIZE => {
            assistant::handle_assistant_materialize(request, state).await
        }
        method::INTEGRATION_INSTALL => {
            integration::handle_integration_install(request, state).await
        }
        method::INTEGRATION_STATUS => integration::handle_integration_status(request, state).await,
        method::INTEGRATION_UNINSTALL => {
            integration::handle_integration_uninstall(request, state).await
        }
        method::INTEGRATION_DOCTOR => integration::handle_integration_doctor(request, state).await,
        method::PACKAGE_LIST => package::handle_package_list(request, state).await,
        method::PACKAGE_INSPECT => package::handle_package_inspect(request, state).await,
        method::PACKAGE_DOCTOR => package::handle_package_doctor(request, state).await,
        method::PACKAGE_INSTALL => package::handle_package_install(request, state).await,
        method::PACKAGE_LINK => package::handle_package_link(request, state).await,
        method::PACKAGE_SET_ENABLED => package::handle_package_set_enabled(request, state).await,
        method::PACKAGE_SELECT => package::handle_package_select(request, state).await,
        method::PACKAGE_UNINSTALL => package::handle_package_uninstall(request, state).await,
        method::PACKAGE_BIND_PROFILE => package::handle_package_bind_profile(request, state).await,
        method::PACKAGE_CREATE_PROFILE => {
            package::handle_package_create_profile(request, state).await
        }
        method::COMMAND_RUN => command::handle_command_run(request, state).await,
        method::HOST_INSPECT => {
            host::handle_host_inspect(request, &state.health, &state.sessions).await
        }
        method::HOST_GOVERNANCE_INSPECT => {
            governance::handle_host_governance_inspect(request, &state.governance).await
        }
        method::HOST_DISCOVER => host::handle_host_discover(request, &state.discovery).await,
        method::HOST_RESOLVE => {
            host::handle_host_resolve(request, &state.discovery, state.transport).await
        }
        method::NOTIFICATION_CREATE => {
            notification::handle_notification_create(
                request,
                state.notifications.as_ref(),
                state.attention.as_ref(),
                &state.sessions,
            )
            .await
        }
        method::NOTIFICATION_LIST => {
            notification::handle_notification_list(request, state.notifications.as_ref()).await
        }
        method::PROJECT_LIST => project::handle_project_list(request, &state.sessions).await,
        method::PROJECT_ADD => project::handle_project_add(request, &state.sessions).await,
        method::PROJECT_SHOW => project::handle_project_show(request, &state.sessions).await,
        method::PROJECT_RENAME => project::handle_project_rename(request, &state.sessions).await,
        method::PROJECT_REMOVE => project::handle_project_remove(request, &state.sessions).await,
        method::PROJECT_PROMPT => project::handle_project_prompt(request, &state.sessions).await,
        method::PROJECT_ACTION => project::handle_project_action(request, &state.sessions).await,
        method::PROJECT_ACTIONS => project::handle_project_actions(request, &state.sessions).await,
        method::PROJECT_CANDIDATES => project::handle_project_candidates(request, state).await,
        method::WORKTREE_REMOVE => worktree::handle_worktree_remove(request, &state.sessions).await,
        other => Response::err(
            selected_version,
            request.id(),
            ProtocolError::method_not_found(other),
        )
        .expect("deserialized request ids satisfy response validation"),
    }
}

fn mutation_target(request: &Request) -> Option<&str> {
    let direct = matches!(
        request.method(),
        method::SESSION_STOP
            | method::SESSION_REMOVE
            | method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED
    );
    let nested = matches!(
        request.method(),
        method::SESSION_RESIZE
            | method::SESSION_SET_METADATA
            | method::SESSION_RENAME
            | method::SESSION_INPUT
    );
    let nested_session_id = || {
        request
            .params()
            .get("session_id")
            .and_then(serde_json::Value::as_str)
    };
    if direct {
        request.params().as_str()
    } else if nested {
        nested_session_id()
    } else if request.method() == method::SESSION_RESUME {
        // A bare id, or the object form that adds the profile-change decision.
        request.params().as_str().or_else(nested_session_id)
    } else {
        None
    }
}

/// Serialize a response to a single JSON line.
///
/// Serialization of our own typed envelopes cannot fail in practice; if it ever
/// did we fall back to a minimal hand-built error line rather than panicking.
pub(crate) fn serialize_response(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|err| {
        warn!(error = %err, "failed to serialize response; sending fallback error");
        format!(
            r#"{{"v":{},"id":"{}","err":{{"class":"daemon","code":"serialize_failed","msg":"response serialization failed"}}}}"#,
            resp.version().get(),
            resp.id().replace('"', "")
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use protocol::{
        method, DetectionRegionKind, Request, RuntimeRef, SessionDetectionParams,
        SessionDetectionResult, SessionId, SessionInfo, SessionNewParams, SessionSetMetadataParams,
        SessionSetMetadataResult, SessionState, StateSource,
    };

    use super::project::live_sessions;
    use super::util::parse_attach_prelude;
    use super::{handle_request, DaemonState, HealthInfo};
    use crate::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};

    /// A minimal `SessionInfo` for the given id/state with a worktree path, so a
    /// test can assert which sessions survive the `project show` live filter.
    fn session(id: &str, state: SessionState) -> SessionInfo {
        let path = PathBuf::from(format!("/work/{id}"));
        SessionInfo {
            blocked_kind: None,
            activity_epoch: None,
            activity_revision: None,
            spawned_by: None,
            attention_isolated_by: None,
            id: SessionId(id.to_owned()),
            external: Some(false),
            name: None,
            agent: "shell".to_owned(),
            agent_base: RuntimeRef::shell(),
            agent_display_name: None,
            cwd: path.clone(),
            cwd_source: Some(protocol::CwdSource::Launch),
            pid: 0,
            runtime: None,
            cols: 80,
            rows: 24,
            state,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            background: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            native_last_activity_at: None,
            project_id: None,
            project_label: None,
            metadata: BTreeMap::new(),
            is_linked_worktree: Some(true),
            repo: None,
            branch: None,
            worktree_path: Some(path),
            warnings: Vec::new(),
            capability_refusals: None,
            placement: None,
            output_force_closed: false,
            created_at: "2026-06-23T00:00:00Z".to_owned(),
            updated_at: "2026-06-23T00:00:00Z".to_owned(),
            ended_at: None,
            exit_code: None,
            capabilities: protocol::SessionCapabilities::default(),
            agent_options: std::collections::BTreeMap::new(),
        }
    }

    fn request(id: &str, method: &str, params: serde_json::Value) -> Request {
        Request::new(id, method, params).expect("valid test request")
    }

    fn ok_value(response: protocol::Response, context: &str) -> serde_json::Value {
        response
            .into_result()
            .unwrap_or_else(|error| panic!("expected {context} success response: {error:?}"))
    }

    fn error_value(response: protocol::Response, context: &str) -> protocol::ProtocolError {
        response
            .into_result()
            .expect_err(&format!("expected {context} error response"))
    }

    fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    // Sessions use the production stop grace, `SessionRegistryConfig::default()`:
    // a shorter grace reaches the forced-stop path on a loaded host, and no test
    // here is about the grace.

    fn daemon_state(health: HealthInfo, sessions: SessionRegistry) -> DaemonState {
        DaemonState::new(
            health,
            sessions,
            Arc::new(crate::governance::HostGovernanceService::open_test()),
            crate::test_support::overlay_registry(),
            super::OverlaySource::Default,
        )
    }

    #[tokio::test]
    async fn origin_guard_denies_every_targeted_session_mutation() {
        let sessions = SessionRegistry::new(SessionRegistryConfig::default());
        let daemon_id = sessions.daemon_instance_id().to_owned();
        let state = daemon_state(HealthInfo::new("test"), sessions);
        let target = SessionId("s-origin".to_owned());
        let mutations = [
            (method::SESSION_STOP, serde_json::json!(target)),
            (method::SESSION_RESUME, serde_json::json!(target)),
            (
                method::SESSION_RESUME,
                serde_json::json!({"session_id": target, "accept_profile_change": true}),
            ),
            (method::SESSION_REMOVE, serde_json::json!(target)),
            (
                method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED,
                serde_json::json!(target),
            ),
            (
                method::SESSION_RESIZE,
                serde_json::json!({"session_id": target}),
            ),
            (
                method::SESSION_SET_METADATA,
                serde_json::json!({"session_id": target}),
            ),
            (
                method::SESSION_RENAME,
                serde_json::json!({"session_id": target}),
            ),
            (
                method::SESSION_INPUT,
                serde_json::json!({"session_id": target}),
            ),
        ];

        for (index, (method, params)) in mutations.into_iter().enumerate() {
            let request = request(&format!("origin-{index}"), method, params)
                .with_origin(Some(target.clone()), Some(daemon_id.clone()))
                .expect("valid origin markers");
            let error = error_value(handle_request(&request, &state).await, method);
            assert_eq!(error.code, "plugin_self_target_denied", "method {method}");
        }
    }

    #[tokio::test]
    async fn origin_guard_allows_other_origins_and_read_only_methods() {
        let sessions = SessionRegistry::new(SessionRegistryConfig::default());
        let daemon_id = sessions.daemon_instance_id().to_owned();
        let state = daemon_state(HealthInfo::new("test"), sessions);
        let target = SessionId("s-origin".to_owned());
        let cases = [
            (
                method::SESSION_STOP,
                serde_json::json!(target),
                SessionId("s-other".to_owned()),
                daemon_id.clone(),
            ),
            (
                method::SESSION_STOP,
                serde_json::json!(target),
                target.clone(),
                "d-other".to_owned(),
            ),
            (
                method::SESSION_INSPECT,
                serde_json::json!(target),
                target.clone(),
                daemon_id.clone(),
            ),
            (
                method::SESSION_REPORT_AGENT,
                serde_json::json!({"session_id": target}),
                target.clone(),
                daemon_id.clone(),
            ),
            (
                method::SESSION_RELEASE_AGENT,
                serde_json::json!({"session_id": target}),
                target.clone(),
                daemon_id.clone(),
            ),
            (
                method::SESSION_REPORT_NATIVE_ID,
                serde_json::json!({"session_id": target}),
                target.clone(),
                daemon_id,
            ),
        ];

        for (index, (method, params, origin_session, origin_daemon)) in
            cases.into_iter().enumerate()
        {
            let request = request(&format!("allowed-origin-{index}"), method, params)
                .with_origin(Some(origin_session), Some(origin_daemon))
                .expect("valid origin markers");
            let error = error_value(handle_request(&request, &state).await, method);
            assert_ne!(error.code, "plugin_self_target_denied", "method {method}");
        }
    }

    #[tokio::test]
    async fn origin_guard_allows_foreign_daemon_to_stop_a_live_session() {
        // The guard verifies which daemon owns the session from the markers in
        // the session process's environment. `/bin/sleep` is a platform binary
        // whose environment macOS 27 withholds, which the guard must read as
        // unverifiable and deny, so the session runs an image whose environment
        // every host exposes.
        let holder = format!("exec {}", pohunek_test_support::hold::shell_words());
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c".to_owned(), holder]),
            ..SessionRegistryConfig::default()
        });
        let id = inert_live_session(&sessions).await;
        let state = daemon_state(HealthInfo::new("test"), sessions.clone());
        let stop = request("foreign-stop", method::SESSION_STOP, serde_json::json!(id))
            .with_origin(Some(id.clone()), Some("d-foreign".to_owned()))
            .expect("valid foreign origin");
        let stopped = ok_value(handle_request(&stop, &state).await, method::SESSION_STOP);
        assert_eq!(stopped["stopped"], true);
        sessions.remove(&id).await.expect("remove stopped session");
    }

    /// Creates a live shell session whose recorded runtime has no definition.
    async fn inert_live_session(sessions: &SessionRegistry) -> SessionId {
        // Ends with the test thread, together with the processes working in it.
        let cwd = crate::test_support::thread_scoped_dir("pohunek-cwd-");
        let created = sessions
            .create(SessionNewParams {
                client_request_id: None,
                agent: "shell".to_owned(),
                name: None,
                cwd: Some(cwd),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                branch_source: None,
                on_diverged: None,
                input_ack_timeout_ms: None,
                metadata: BTreeMap::new(),
                secret_env: protocol::SessionSecretEnv::default(),
                context_package_digest: None,
                fetch_ref: None,
                agent_options: std::collections::BTreeMap::new(),
            })
            .await
            .expect("create session");
        sessions
            .set_agent_base_for_test(&created.id, RuntimeRef::from_wire("acme"))
            .await;
        created.id
    }

    #[tokio::test]
    async fn stop_and_remove_dispatch_release_a_session_whose_runtime_is_not_installed() {
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", SLEEP_SESSION_SCRIPT]),
            ..SessionRegistryConfig::default()
        });
        let id = inert_live_session(&sessions).await;
        let state = daemon_state(HealthInfo::new("test"), sessions.clone());

        let input = request(
            "inert-input",
            method::SESSION_INPUT,
            serde_json::json!({"session_id": id, "text": "x"}),
        );
        let refused = error_value(handle_request(&input, &state).await, "session.input");
        assert_eq!(refused.code, "runtime_not_installed");

        let stop = request("inert-stop", method::SESSION_STOP, serde_json::json!(id));
        let stopped = ok_value(handle_request(&stop, &state).await, "session.stop");
        assert_eq!(stopped["stopped"], true);

        let remove = request(
            "inert-remove",
            method::SESSION_REMOVE,
            serde_json::json!(id),
        );
        let removed = ok_value(handle_request(&remove, &state).await, "session.remove");
        assert_eq!(removed["removed"], true);
        sessions
            .inspect(&id)
            .await
            .expect_err("the session is gone");
    }

    #[tokio::test]
    async fn remove_dispatch_of_a_live_session_with_an_uninstalled_runtime_stops_its_worker() {
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", SLEEP_SESSION_SCRIPT]),
            ..SessionRegistryConfig::default()
        });
        let id = inert_live_session(&sessions).await;
        let state = daemon_state(HealthInfo::new("test"), sessions.clone());

        let remove = request(
            "inert-remove",
            method::SESSION_REMOVE,
            serde_json::json!(id),
        );
        let removed = ok_value(handle_request(&remove, &state).await, "session.remove");

        assert_eq!(removed["removed"], true);
        assert_eq!(removed["stopped"], true);
        sessions
            .inspect(&id)
            .await
            .expect_err("the session is gone");
    }

    #[tokio::test]
    async fn session_set_metadata_dispatch_updates_session() {
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", SLEEP_SESSION_SCRIPT]),
            ..SessionRegistryConfig::default()
        });
        let cwd = pohunek_test_support::tempdir().expect("create the session working directory");
        let created = sessions
            .create(SessionNewParams {
                client_request_id: None,
                agent: "shell".to_owned(),
                name: None,
                cwd: Some(cwd.path().to_path_buf()),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                branch_source: None,
                on_diverged: None,
                input_ack_timeout_ms: None,
                metadata: metadata(&[("owner", "cli")]),
                secret_env: protocol::SessionSecretEnv::default(),
                context_package_digest: None,
                fetch_ref: None,
                agent_options: std::collections::BTreeMap::new(),
            })
            .await
            .expect("create session");
        let state = daemon_state(HealthInfo::new("test"), sessions.clone());
        let params = SessionSetMetadataParams {
            session_id: created.id.clone(),
            metadata: BTreeMap::from([
                ("owner".to_owned(), Some("daemon".to_owned())),
                ("ticket".to_owned(), Some("DMD-1356".to_owned())),
            ]),
        };
        let request = request(
            "set-metadata",
            method::SESSION_SET_METADATA,
            serde_json::to_value(params).expect("params serialize"),
        );

        let response = handle_request(&request, &state).await;

        let ok = ok_value(response, "session.set_metadata");
        let result: SessionSetMetadataResult =
            serde_json::from_value(ok).expect("result deserializes");
        assert_eq!(
            result.session.metadata,
            metadata(&[("owner", "daemon"), ("ticket", "DMD-1356")])
        );
        assert_eq!(
            sessions
                .inspect(&created.id)
                .await
                .expect("inspect")
                .metadata,
            result.session.metadata
        );

        sessions.stop(&created.id).await.expect("stop the session");
    }

    #[tokio::test]
    async fn session_detection_dispatch_returns_active_manifest_previews() {
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", SLEEP_SESSION_SCRIPT]),
            ..SessionRegistryConfig::default()
        });
        let cwd = pohunek_test_support::tempdir().expect("create the session working directory");
        let created = sessions
            .create(SessionNewParams {
                client_request_id: None,
                agent: "shell".to_owned(),
                name: None,
                cwd: Some(cwd.path().to_path_buf()),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                branch_source: None,
                on_diverged: None,
                input_ack_timeout_ms: None,
                metadata: BTreeMap::new(),
                secret_env: protocol::SessionSecretEnv::default(),
                context_package_digest: None,
                fetch_ref: None,
                agent_options: std::collections::BTreeMap::new(),
            })
            .await
            .expect("create shell session");
        let state = daemon_state(HealthInfo::new("test"), sessions.clone());
        let request = request(
            "detection-preview",
            method::SESSION_DETECTION,
            serde_json::to_value(SessionDetectionParams::new(created.id.clone()))
                .expect("params serialize"),
        );

        let result: SessionDetectionResult = serde_json::from_value(ok_value(
            handle_request(&request, &state).await,
            "session.detection",
        ))
        .expect("result deserializes");

        assert_eq!(result.session_id, created.id);
        assert_eq!(
            result.supported_regions,
            DetectionRegionKind::ALL
                .into_iter()
                .filter(|kind| *kind != DetectionRegionKind::ProgramStatus)
                .collect::<Vec<_>>(),
            "a manifest without the program-status table lists no such region"
        );
        assert_eq!(
            result
                .previews
                .iter()
                .map(|preview| preview.kind)
                .collect::<Vec<_>>(),
            vec![
                DetectionRegionKind::OscTitle,
                DetectionRegionKind::WholeRecent,
                DetectionRegionKind::OscProgress,
            ]
        );
        sessions.stop(&created.id).await.expect("stop the session");
    }

    #[tokio::test]
    async fn session_detection_dispatch_rejects_unknown_session() {
        let state = daemon_state(
            HealthInfo::new("test"),
            SessionRegistry::new(SessionRegistryConfig::default()),
        );
        let request = request(
            "detection-missing",
            method::SESSION_DETECTION,
            serde_json::to_value(SessionDetectionParams::new(SessionId("missing".to_owned())))
                .expect("params serialize"),
        );

        let error = error_value(
            handle_request(&request, &state).await,
            "session.detection missing",
        );

        assert_eq!(error.code, "session_not_found");
    }

    fn attention_create_request(id: &str) -> Request {
        let params = protocol::NotificationCreateParams {
            source: protocol::NotificationSource {
                provider: "scenario-tool".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: "codex-hook-s-1".to_owned(),
            },
            kind: protocol::NotificationKind::ApprovalRequired,
            severity: protocol::NotificationSeverity::ActionRequired,
            title: "Approval required".to_owned(),
            body: "Codex is waiting for a tool approval.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: Some(SessionId("s-1".to_owned())),
            agent_kind: Some(RuntimeRef::codex()),
            source_id: Some("codex:s-1:permission:1".to_owned()),
            dedupe_key: Some("attention:s-1".to_owned()),
            project_id: Some("p-1".to_owned()),
            retryable_admission: false,
        };
        request(
            id,
            method::NOTIFICATION_CREATE,
            serde_json::to_value(params).expect("params serialize"),
        )
    }

    fn hook_approval_params(host_local_source_id: &str) -> protocol::NotificationCreateParams {
        protocol::NotificationCreateParams {
            source: protocol::NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: host_local_source_id.to_owned(),
            },
            kind: protocol::NotificationKind::ApprovalRequired,
            severity: protocol::NotificationSeverity::ActionRequired,
            title: "Approval required".to_owned(),
            body: "Codex is waiting for a tool approval.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: None,
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            retryable_admission: false,
        }
    }

    fn error_create_request(id: &str) -> Request {
        let params = protocol::NotificationCreateParams {
            source: protocol::NotificationSource {
                provider: "scenario-tool".to_owned(),
                provider_event: "Error".to_owned(),
                host_local_source_id: "codex-error-s-1".to_owned(),
            },
            kind: protocol::NotificationKind::Error,
            severity: protocol::NotificationSeverity::Error,
            title: "Agent error".to_owned(),
            body: "Codex reported an error.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: Some(SessionId("s-1".to_owned())),
            agent_kind: Some(RuntimeRef::codex()),
            source_id: Some("codex:s-1:error:1".to_owned()),
            dedupe_key: None,
            project_id: Some("p-1".to_owned()),
            retryable_admission: false,
        };
        request(
            id,
            method::NOTIFICATION_CREATE,
            serde_json::to_value(params).expect("params serialize"),
        )
    }

    fn turn_create_request(id: &str) -> Request {
        let params = protocol::NotificationCreateParams {
            source: protocol::NotificationSource {
                provider: "scenario-tool".to_owned(),
                provider_event: "Stop".to_owned(),
                host_local_source_id: "codex-stop-s-1".to_owned(),
            },
            kind: protocol::NotificationKind::TurnCompleted,
            severity: protocol::NotificationSeverity::Info,
            title: "Turn completed".to_owned(),
            body: "Codex completed a turn.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: Some(SessionId("s-1".to_owned())),
            agent_kind: Some(RuntimeRef::codex()),
            source_id: Some("codex:s-1:stop:1".to_owned()),
            dedupe_key: Some("turn:s-1".to_owned()),
            project_id: Some("p-1".to_owned()),
            retryable_admission: false,
        };
        request(
            id,
            method::NOTIFICATION_CREATE,
            serde_json::to_value(params).expect("params serialize"),
        )
    }

    async fn create_result(
        state: &DaemonState,
        request: &Request,
    ) -> protocol::NotificationCreateResult {
        let response = handle_request(request, state).await;
        let ok = ok_value(response, "notification.create");
        serde_json::from_value(ok).expect("create result deserializes")
    }

    async fn list_records(state: &DaemonState) -> Vec<protocol::NotificationRecord> {
        let request = request(
            "notification-list",
            method::NOTIFICATION_LIST,
            serde_json::to_value(protocol::NotificationListParams::default())
                .expect("list params serialize"),
        );
        let response = handle_request(&request, state).await;
        let ok = ok_value(response, "notification.list");
        let result: protocol::NotificationListResult =
            serde_json::from_value(ok).expect("list result");
        result.notifications
    }

    /// A provider hook notification that reaches the daemon after the worker
    /// runs and before the session's entry is installed is stored, linked to
    /// the launching session; after the install the session is held.
    #[tokio::test]
    async fn a_hook_notification_in_the_launch_window_is_stored() {
        use crate::notifications::NotificationService;

        let notification_dir =
            pohunek_test_support::tempdir_with_prefix("ph-notify-").expect("notification dir");
        let notifications =
            NotificationService::open(notification_dir.path()).expect("notification service opens");
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", SLEEP_SESSION_SCRIPT]),
            ..SessionRegistryConfig::default()
        });
        let state = daemon_state(HealthInfo::new("test"), sessions.clone())
            .with_notifications(notifications);

        let gate = sessions.hold_next_launch_commit();
        let cwd = crate::test_support::thread_scoped_dir("pohunek-cwd-");
        let creating = tokio::spawn({
            let sessions = sessions.clone();
            async move {
                sessions
                    .create(SessionNewParams {
                        client_request_id: None,
                        agent: "shell".to_owned(),
                        name: None,
                        cwd: Some(cwd),
                        cols: 80,
                        rows: 24,
                        project: None,
                        repo: None,
                        branch: None,
                        base_branch: None,
                        input: None,
                        extended_input_ready_wait: None,
                        branch_source: None,
                        on_diverged: None,
                        input_ack_timeout_ms: None,
                        metadata: BTreeMap::new(),
                        secret_env: protocol::SessionSecretEnv::default(),
                        context_package_digest: None,
                        fetch_ref: None,
                        agent_options: std::collections::BTreeMap::new(),
                    })
                    .await
            }
        });
        tokio::time::timeout(
            pohunek_test_support::wait::HANG_GUARD,
            gate.entered.notified(),
        )
        .await
        .expect("the launch parks before its entry is installed");
        let launches = sessions.launching_for_test();
        let [launching] = launches.as_slice() else {
            panic!("one launch is in flight: {launches:?}");
        };
        let launching = launching.clone();
        assert!(
            !sessions.holds(&launching).await,
            "the entry is not installed yet"
        );

        let mut params = hook_approval_params("window-report");
        params.session_id = Some(launching.clone());
        let created = create_result(
            &state,
            &request(
                "window-create",
                method::NOTIFICATION_CREATE,
                serde_json::to_value(params).expect("params serialize"),
            ),
        )
        .await;
        assert!(created.created);
        assert_eq!(created.record.session_id, Some(launching.clone()));
        let listed = list_records(&state).await;
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].id, created.record.id);

        gate.release.notify_one();
        let session = creating
            .await
            .expect("create task joins")
            .expect("the launch completes");
        assert_eq!(session.id, launching);
        assert!(sessions.holds(&launching).await);
        assert!(sessions.launching_for_test().is_empty());
        sessions.stop(&launching).await.expect("stop the session");
    }

    #[tokio::test(start_paused = true)]
    async fn notification_create_defers_attention_and_stores_no_turn_report() {
        use crate::notifications::{AttentionCoordinator, NotificationService};
        use protocol::NotificationKind;

        let notification_dir =
            pohunek_test_support::tempdir_with_prefix("ph-notify-").expect("notification dir");
        let notifications =
            NotificationService::open(notification_dir.path()).expect("notification service opens");
        let sessions = SessionRegistry::new(SessionRegistryConfig::default());
        let (attention, _task) = AttentionCoordinator::spawn(
            &crate::shutdown::DaemonShutdown::default(),
            notifications.clone(),
            &sessions,
        );
        let state = daemon_state(HealthInfo::new("test"), sessions)
            .with_notifications(notifications)
            .with_attention_coordinator(attention);

        // Attention create: the handler mints and returns the record, but holds it
        // pending, so it is not yet listable.
        let created = create_result(&state, &attention_create_request("attention-create")).await;
        assert!(created.created);
        assert_eq!(created.record.kind, NotificationKind::ApprovalRequired);
        assert!(
            list_records(&state).await.is_empty(),
            "an attention create must be debounced, not immediately listable"
        );

        // A turn report is never stored: the answer carries the minted record
        // with `created: false`, now and after the debounce window.
        let turn = create_result(&state, &turn_create_request("turn-create")).await;
        assert!(!turn.created);
        assert_eq!(turn.record.kind, NotificationKind::TurnCompleted);

        // Any other kind is accepted and dropped.
        let error = create_result(&state, &error_create_request("error-create")).await;
        assert!(!error.created);
        assert!(
            list_records(&state).await.is_empty(),
            "neither a turn report nor a dropped kind is listable"
        );

        // After the debounce window the held attention record surfaces too.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(
            pohunek_service_config::DEFAULT_NOTIFICATION_SETTINGS.attention_debounce_secs + 1,
        ))
        .await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        let flushed = list_records(&state).await;
        assert_eq!(
            flushed.len(),
            1,
            "only the debounced attention record flushes"
        );
        assert!(flushed.iter().any(|record| record.id == created.record.id
            && record.kind == NotificationKind::ApprovalRequired));
    }

    /// A shell session body that stays alive without a `PATH` lookup, so a test
    /// that overrides `PATH` cannot make the session exit early.
    const SLEEP_SESSION_SCRIPT: &str = "exec /bin/sleep 30";

    #[test]
    fn live_sessions_keeps_only_non_terminal_sessions() {
        let infos = vec![
            session("starting", SessionState::Starting),
            session("running", SessionState::Running),
            session("stopped", SessionState::Stopped),
            session("done", SessionState::Done),
            session("failed", SessionState::Failed),
        ];

        let live = live_sessions(infos);

        // Starting + Running survive; the three terminal states are dropped, so a
        // stopped session can no longer mark its worktree as occupied in `show`.
        let ids: Vec<&str> = live.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["starting", "running"]);
    }

    #[test]
    fn attach_prelude_requires_exact_one_field_shape() {
        assert_eq!(
            parse_attach_prelude(r#"{"attach":"a-1"}"#),
            Some("a-1".to_owned())
        );
        assert_eq!(parse_attach_prelude(r#"{"attach":""}"#), None);
        assert_eq!(
            parse_attach_prelude(
                r#"{"v":1,"id":"req-1","method":"daemon.health","params":null,"attach":"a-1"}"#
            ),
            None
        );
    }

    fn idle_state() -> DaemonState {
        daemon_state(
            HealthInfo::new("test"),
            SessionRegistry::new(SessionRegistryConfig::default()),
        )
    }

    #[tokio::test]
    async fn a_version_below_the_window_is_rejected_before_any_method_runs() {
        let below = protocol::MIN_PROTOCOL_VERSION.get() - 1;
        let request: Request = serde_json::from_value(serde_json::json!({
            "v": {"minimum": below, "maximum": below},
            "id": "below-1",
            "method": method::DAEMON_HEALTH,
            "params": null,
        }))
        .expect("valid request");

        let response = handle_request(&request, &idle_state()).await;

        assert_eq!(response.version(), protocol::PROTOCOL_VERSION);
        let error = error_value(response, "daemon.health");
        assert_eq!(error.code, "version_mismatch");
    }
}
