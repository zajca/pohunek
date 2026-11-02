//! Client-side assistant launch orchestration.
//!
//! Agent selection, knowledge materialization, prompt composition and the
//! `session.new` call shared by every pohunek client. The daemon only sees
//! ordinary protocol requests.

// Rust guideline compliant 2026-10-03

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use knowledge::{
    assistant_keyed_launch_id, assistant_launch_id, bundle_content_hash, bundle_index,
    materialize as materialize_bundle, materialized_version_hash, BUNDLE_VERSION,
};
use pohunek_client::{session_new_request_deadline, Client, ClientError, DEFAULT_REQUEST_TIMEOUT};
use protocol::{
    method, AssistantMaterializeParams, AssistantMaterializeResult, ConceptIntent, ConceptMeta,
    HostCapabilities, ProtocolError, RuntimeRef, SessionInfo, SessionNewParams, SessionNewResult,
};
use serde::Serialize;

use crate::{
    connect_client, runtime_is_assistant_capable, AssistantError, ConnectionOptions, HostConfig,
    HostTransport,
};

const SNAPSHOT_FILE: &str = "snapshot.json";
/// Assistant launch intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Setup,
    Project,
    Update,
    Debug,
    Help,
}

impl Intent {
    /// Return the stable lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Project => "project",
            Self::Update => "update",
            Self::Debug => "debug",
            Self::Help => "help",
        }
    }

    const fn as_concept_intent(self) -> ConceptIntent {
        match self {
            Self::Setup => ConceptIntent::Setup,
            Self::Project => ConceptIntent::Project,
            Self::Update => ConceptIntent::Update,
            Self::Debug => ConceptIntent::Debug,
            Self::Help => ConceptIntent::Help,
        }
    }
}

impl std::fmt::Display for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Local paths needed for assistant materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantPaths {
    pub runtime_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl AssistantPaths {
    /// Resolve assistant paths from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`AssistantError::MissingEnv`] when the required XDG or `HOME`
    /// environment variables are absent.
    pub fn resolve() -> Result<Self, AssistantError> {
        let paths = pohunek_paths::BasePaths::resolve().map_err(path_error)?;

        Ok(Self {
            runtime_dir: paths.runtime_dir,
            data_dir: paths.data_dir,
            log_dir: paths.log_dir,
            cache_dir: paths.cache_dir,
            config_dir: paths.config_dir,
        })
    }

    /// Return the local knowledge bundle cache directory.
    #[must_use]
    pub fn assistant_bundle_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(pohunek_paths::KNOWLEDGE_CACHE_SUBDIR)
    }

    fn assistant_runtime_dir(&self, launch_id: &str) -> Option<PathBuf> {
        pohunek_paths::valid_path_id(launch_id).map(|id| {
            self.runtime_dir
                .join(pohunek_paths::ASSISTANT_RUNTIME_SUBDIR)
                .join(id)
        })
    }
}

/// Inputs for preparing and launching an assistant session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchParams {
    pub intent: Intent,
    pub request: Option<String>,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub repo: Option<PathBuf>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub branch_source: Option<protocol::BranchSource>,
    pub on_diverged: Option<protocol::OnDiverged>,
    pub cols: u16,
    pub rows: u16,
    pub no_snapshot: bool,
    pub degraded: bool,
    pub auto_started_daemon: bool,
    /// Caller-chosen idempotency key of the launch, to resume an earlier launch
    /// whose outcome was unknown. `None` lets a daemon that supports keys get a
    /// generated one.
    pub request_id: Option<String>,
}

/// Materialized assistant knowledge description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeInfo {
    pub bundle_path: Option<String>,
    pub snapshot_path: String,
    pub version: String,
    pub label: &'static str,
}

/// Prepared prompt and session parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLaunch {
    pub intent: Intent,
    pub selection: AgentSelection,
    pub knowledge: KnowledgeInfo,
    pub prompt: String,
    pub session_params: SessionNewParams,
    pub snapshot_included: bool,
    pub auto_started_daemon: bool,
}

/// Assistant metadata returned with a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchMeta {
    pub intent: Intent,
    pub agent: String,
    pub knowledge_bundle_version: String,
    pub snapshot_included: bool,
    pub auto_started_daemon: bool,
    pub knowledge: &'static str,
}

/// Result of starting an assistant session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResult {
    pub session: SessionInfo,
    pub applied_input: Option<bool>,
    pub worktree_source: Option<protocol::WorktreeSource>,
    /// The daemon answered with a session an earlier attempt of the same
    /// request had created, so no new session exists.
    pub replayed: bool,
    pub assistant: LaunchMeta,
}

/// Selected assistant agent runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSelection {
    pub name: String,
    pub reason: String,
}

/// Agents preferred for assistant sessions.
///
/// A user profile named `pohunek-assistant` is intentionally first so the
/// operator can specialize the assistant without changing launch code.
const RANKED_AGENTS: [&str; 4] = ["pohunek-assistant", "codex", "claude", "hermes"];

/// Resolve which agent should run the assistant.
///
/// An explicit choice is rejected when the host reports its version policy as
/// refusing it; other explicit names remain daemon-authoritative for profile
/// resolution and launch validation.
/// Automatic choices must be available supported non-shell runtimes.
///
/// # Errors
///
/// Returns [`ProtocolError::no_capable_agent`] when no available non-shell
/// runtime is reported by the host.
pub fn select_agent(
    capabilities: &HostCapabilities,
    requested: Option<&str>,
) -> Result<AgentSelection, ProtocolError> {
    if let Some(agent) = requested {
        if explicit_agent_is_allowed(capabilities, agent) {
            return Ok(AgentSelection {
                name: agent.to_owned(),
                reason: "selected explicitly".to_owned(),
            });
        }

        return Err(ProtocolError::no_capable_agent());
    }

    for candidate in RANKED_AGENTS {
        if is_assistant_runtime(capabilities, candidate) {
            return Ok(AgentSelection {
                name: candidate.to_owned(),
                reason: format!("highest-ranked available runtime ({candidate})"),
            });
        }
    }

    if let Some(runtime) = capabilities
        .runtimes
        .iter()
        .find(|runtime| is_assistant_runtime(capabilities, &runtime.agent))
    {
        return Ok(AgentSelection {
            name: runtime.agent.clone(),
            reason: format!("available host runtime ({})", runtime.agent),
        });
    }

    Err(ProtocolError::no_capable_agent())
}

/// Prepare an assistant launch without starting the session.
///
/// # Errors
///
/// Returns [`AssistantError`] when target validation, host inspection, snapshot
/// materialization, read preflight, or prompt construction fails.
pub async fn prepare_with_options(
    config: &HostConfig,
    paths: &AssistantPaths,
    params: LaunchParams,
    options: ConnectionOptions,
) -> Result<PreparedLaunch, AssistantError> {
    validate_target(config, &params)?;
    let mut client = connect_client(config, options).await?;
    let capabilities = fetch_capabilities(&mut client).await?;
    if params.branch_source.is_some() && !capabilities.session_new_branch_source_supported {
        return Err(protocol::ProtocolError::new(
            protocol::ErrorClass::Configuration,
            "session_new_branch_source_unsupported",
            format!(
                "host's daemon {} does not advertise `session_new_branch_source_supported`: \
                 --from-remote needs a daemon that advertises it; no session was created",
                capabilities.daemon_version
            ),
            Some(
                "update and restart the pohunek daemon on that host, or start without --from-remote"
                    .to_owned(),
            ),
        )
        .into());
    }
    let client_request_id = launch_request_id(config, &capabilities, params.request_id.as_deref())?;
    let selection = select_agent(&capabilities, params.agent.as_deref())?;
    let (snapshot, orientation) = if params.no_snapshot {
        skipped_snapshot(config, &params, &selection.name)
    } else {
        collect_snapshot(config, &mut client, &params, &selection.name)
    };

    let knowledge = if params.degraded {
        let degraded = materialize_degraded(paths, &snapshot, client_request_id.as_deref())?;
        preflight_snapshot_readable(&degraded.snapshot_path)?;
        PreparedKnowledge::Degraded(degraded)
    } else if is_remote(config) {
        PreparedKnowledge::Materialized(materialize_remote(&mut client, &snapshot).await?)
    } else {
        let materialized = materialize_local(paths, &snapshot, client_request_id.as_deref())?;
        preflight_read_access(
            &materialized.bundle_path,
            &materialized.snapshot_path,
            false,
        )?;
        PreparedKnowledge::Materialized(materialized)
    };

    let contract = embedded_contract()?;
    let prompt = match &knowledge {
        PreparedKnowledge::Materialized(materialized) => compose(&ComposeParams {
            contract: &contract,
            intent: params.intent,
            request: params.request.as_deref(),
            concepts: &materialized.concepts,
            bundle_path: &materialized.bundle_path,
            snapshot_path: &materialized.snapshot_path,
            orientation: &orientation,
            version: &materialized.version,
        }),
        PreparedKnowledge::Degraded(degraded) => compose_degraded(&ComposeDegradedParams {
            contract: &contract,
            intent: params.intent,
            request: params.request.as_deref(),
            snapshot_path: &degraded.snapshot_path,
            orientation: &orientation,
            version: &degraded.version,
            bundle_version_note: &degraded.version,
        }),
    };

    let session_params = SessionNewParams {
        client_request_id,
        agent: selection.name.clone(),
        name: None,
        cwd: None,
        cols: params.cols,
        rows: params.rows,
        project: params.project,
        repo: params.repo,
        branch: params.branch,
        base_branch: params.base_branch,
        input: Some(prompt.clone()),
        extended_input_ready_wait: None,
        branch_source: params.branch_source,
        on_diverged: params.on_diverged,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
        secret_env: protocol::SessionSecretEnv::default(),
    };

    Ok(PreparedLaunch {
        intent: params.intent,
        selection,
        knowledge: knowledge.info(),
        prompt,
        session_params,
        snapshot_included: !params.no_snapshot,
        auto_started_daemon: params.auto_started_daemon,
    })
}

/// Start an already prepared assistant launch.
///
/// # Errors
///
/// Returns [`AssistantError`] when `session.new` fails.
pub async fn start_prepared_with_options(
    config: &HostConfig,
    prepared: PreparedLaunch,
    options: ConnectionOptions,
) -> Result<LaunchResult, AssistantError> {
    let result = create_session(config, &prepared.session_params, options).await?;

    Ok(LaunchResult {
        session: result.session,
        applied_input: result.applied_input,
        worktree_source: result.worktree_source,
        replayed: result.replayed == Some(true),
        assistant: LaunchMeta {
            intent: prepared.intent,
            agent: prepared.selection.name,
            knowledge_bundle_version: prepared.knowledge.version,
            snapshot_included: prepared.snapshot_included,
            auto_started_daemon: prepared.auto_started_daemon,
            knowledge: prepared.knowledge.label,
        },
    })
}

/// Sends `session.new`, once without a key and at most twice with one.
///
/// A keyed request is safe to repeat: the daemon answers a repeat with the
/// session the first one created, with `session_request_in_progress` while it
/// is still being created, or creates it when the first never arrived. One
/// budget `D` (the explicit request timeout, or the dedicated creation budget)
/// is measured from the first send. The first attempt gets `D` minus a reserve
/// of [`DEFAULT_REQUEST_TIMEOUT`] (at most half of `D`), the retry what remains,
/// and each attempt as a whole (connections, capability check and request) is
/// bounded by its share, so the two never exceed `D`. Only an unanswered attempt
/// is retried; a typed daemon error is final.
async fn create_session(
    config: &HostConfig,
    params: &SessionNewParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, AssistantError> {
    let Some(request_id) = params.client_request_id.as_deref() else {
        return attempt(config, params, options).await;
    };
    let resume = ResumeHint {
        request_id,
        resumable: !is_remote(config),
    };
    let started = tokio::time::Instant::now();
    let deadline = session_new_request_deadline(params, options.request_timeout);
    let reserve = DEFAULT_REQUEST_TIMEOUT.min(deadline / 2);
    let first = bounded_attempt(config, params, options, deadline.saturating_sub(reserve)).await;
    let unanswered = match first {
        Ok(result) => return Ok(result),
        Err(error) if outcome_unknown(&error) => error,
        Err(error) => return Err(resume.name(error)),
    };
    let remaining = deadline.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(resume.name(unanswered));
    }
    bounded_attempt(config, params, options, remaining)
        .await
        .map_err(|error| resume.name(error))
}

/// One attempt that ends within `limit`, connections included.
async fn bounded_attempt(
    config: &HostConfig,
    params: &SessionNewParams,
    options: ConnectionOptions,
    limit: Duration,
) -> Result<SessionNewResult, AssistantError> {
    let attempted = tokio::time::timeout(
        limit,
        attempt(config, params, options.with_request_timeout(limit)),
    )
    .await;
    attempted.unwrap_or_else(|_elapsed| {
        Err(AssistantError::Client(ClientError::RequestTimeout {
            host: is_remote(config).then(|| host_label(config)),
            timeout: limit,
        }))
    })
}

/// One `session.new` on a fresh connection.
async fn attempt(
    config: &HostConfig,
    params: &SessionNewParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, AssistantError> {
    let mut client = connect_client(config, options).await?;
    Ok(client.call::<method::SessionNew>(params.clone()).await?)
}

/// Whether `error` leaves it unknown that the daemon handled the request: a
/// deadline, a connection that failed or dropped, or a reply that cannot be
/// read. A keyed request is safe to repeat in each of these cases.
const fn outcome_unknown(error: &AssistantError) -> bool {
    matches!(
        error,
        AssistantError::Client(
            ClientError::RequestTimeout { .. }
                | ClientError::Framing(_)
                | ClientError::Io(_)
                | ClientError::Json(_)
                | ClientError::DaemonUnreachable { .. }
                | ClientError::HostUnreachable { .. }
                | ClientError::RemoteDaemonUnavailable { .. }
        )
    )
}

/// Typed `session.new` errors that can be returned after the session was
/// recorded, so the caller may need the request id to get it back.
const RECORDED_CODES: [&str; 2] = [
    "session_request_in_progress",
    "session_runtime_reconnecting",
];

/// How a launch that carried a request id tells its caller to get the session
/// back.
struct ResumeHint<'a> {
    request_id: &'a str,
    /// A new process renders the same opening prompt under the id, so the daemon
    /// replays. A remote daemon picks a different snapshot path for every launch,
    /// so only the retry inside the launch can replay there.
    resumable: bool,
}

impl ResumeHint<'_> {
    /// `error` with the request id in its message and a way to get the session
    /// back in its recovery. A typed daemon error keeps its own recovery first.
    fn name(&self, error: AssistantError) -> AssistantError {
        let unknown = outcome_unknown(&error);
        let mut error = match error {
            AssistantError::Client(source) => source.to_protocol_error(),
            AssistantError::Protocol(source) => source,
            other => return other,
        };
        error.msg = format!("{} (request id `{}`)", error.msg, self.request_id);
        let resume = self.recover_text();
        let recorded = RECORDED_CODES.contains(&error.code.as_str());
        error.recover = match (unknown, recorded, error.recover.take()) {
            (true, _, _) | (false, true, None) => Some(resume),
            (false, true, Some(own)) => {
                Some(format!("{own}; if a session was created anyway, {resume}"))
            }
            (false, false, own) => own,
        };
        AssistantError::Protocol(error)
    }

    fn recover_text(&self) -> String {
        let id = self.request_id;
        if self.resumable {
            format!(
                "the daemon may have created the session: rerun the same command with \
                 `--request-id {id}` to get it back (the daemon answers with the same session, \
                 or with `session_request_in_progress` while it is still being created); the \
                 request, target and `--meta` flags and the agent must be unchanged"
            )
        } else {
            format!(
                "the daemon may have created the session: look for it with `pohunek session \
                 list`, filtered by the `--meta` key you passed; a new launch cannot replay \
                 request id `{id}` on a remote host, because the opening prompt names a snapshot \
                 path that host picks for every launch"
            )
        }
    }
}

/// The idempotency key the launch sends, or `None` for a daemon that cannot
/// honor one.
///
/// A caller key is checked against the daemon's capability, since an old daemon
/// would drop it silently. Without one, a daemon that supports keys gets a
/// generated key so a lost reply can be retried safely.
fn launch_request_id(
    config: &HostConfig,
    capabilities: &HostCapabilities,
    requested: Option<&str>,
) -> Result<Option<String>, AssistantError> {
    if let Some(id) = requested {
        if !protocol::is_valid_client_request_id(id) {
            return Err(ProtocolError::bad_request(
                "request id must be 1 to 128 bytes of A-Z, a-z, 0-9, '.', '_', ':' or '-'",
            )
            .into());
        }
        if !capabilities.session_new_request_id_supported {
            let host = is_remote(config).then(|| host_label(config));
            return Err(ProtocolError::session_new_request_id_unsupported(host.as_deref()).into());
        }
        return Ok(Some(id.to_owned()));
    }
    if !capabilities.session_new_request_id_supported {
        return Ok(None);
    }
    generated_request_id().map(Some)
}

/// `asst-` and 128 random bits as 32 lowercase hex digits.
fn generated_request_id() -> Result<String, AssistantError> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| {
        AssistantError::Protocol(ProtocolError::new(
            protocol::ErrorClass::Runtime,
            "assistant_launch_failed",
            format!("cannot generate a request id: {error}"),
            None,
        ))
    })?;
    let mut id = String::from("asst-");
    for byte in bytes {
        write!(id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(id)
}

/// Prepare and start an assistant session.
///
/// # Errors
///
/// Returns [`AssistantError`] when preparation or `session.new` fails.
pub async fn launch_with_options(
    config: &HostConfig,
    paths: &AssistantPaths,
    params: LaunchParams,
    options: ConnectionOptions,
) -> Result<LaunchResult, AssistantError> {
    let prepared = prepare_with_options(config, paths, params, options).await?;
    start_prepared_with_options(config, prepared, options).await
}

fn is_assistant_runtime(capabilities: &HostCapabilities, agent: &str) -> bool {
    capabilities
        .runtimes
        .iter()
        .any(|runtime| runtime.agent == agent && runtime_is_assistant_capable(runtime))
}

fn explicit_agent_is_allowed(capabilities: &HostCapabilities, agent: &str) -> bool {
    let Some(runtime) = capabilities
        .runtimes
        .iter()
        .find(|runtime| runtime.agent == agent)
    else {
        return true;
    };
    // A refused version policy or a historical base is decided by the host's
    // report; every other case stays with the daemon's own launch checks.
    runtime.supported != Some(false)
        && !matches!(runtime.agent_base.as_ref(), Some(RuntimeRef::Historical(_)))
}

fn validate_target(config: &HostConfig, params: &LaunchParams) -> Result<(), AssistantError> {
    if !is_remote(config) {
        return Ok(());
    }
    let host = host_label(config);
    if params.degraded {
        return Err(AssistantError::RemoteAssistantDegradedUnsupported { host });
    }
    if params.project.is_none() && params.repo.is_none() {
        return Err(AssistantError::RemoteAssistantTargetRequired { host });
    }
    Ok(())
}

fn is_remote(config: &HostConfig) -> bool {
    !matches!(config.transport, HostTransport::Local { .. })
}

fn host_label(config: &HostConfig) -> String {
    match &config.transport {
        HostTransport::Local { .. } => "local".to_owned(),
        HostTransport::Remote { host, .. } | HostTransport::Pinned { host, .. } => host.clone(),
        HostTransport::Tcp { .. } => config.id.as_str().to_owned(),
    }
}

async fn fetch_capabilities(client: &mut Client) -> Result<HostCapabilities, AssistantError> {
    Ok(client.call::<method::HostInspect>(()).await?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DegradedMaterializeResult {
    snapshot_path: String,
    version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreparedKnowledge {
    Materialized(AssistantMaterializeResult),
    Degraded(DegradedMaterializeResult),
}

impl PreparedKnowledge {
    fn info(&self) -> KnowledgeInfo {
        match self {
            Self::Materialized(materialized) => KnowledgeInfo {
                bundle_path: Some(materialized.bundle_path.clone()),
                snapshot_path: materialized.snapshot_path.clone(),
                version: materialized.version.clone(),
                label: "materialized",
            },
            Self::Degraded(degraded) => KnowledgeInfo {
                bundle_path: None,
                snapshot_path: degraded.snapshot_path.clone(),
                version: degraded.version.clone(),
                label: "degraded",
            },
        }
    }
}

fn materialize_degraded(
    paths: &AssistantPaths,
    snapshot: &str,
    request_id: Option<&str>,
) -> Result<DegradedMaterializeResult, AssistantError> {
    let runtime_dir = assistant_runtime_dir(paths, request_id)?;
    create_private_dir_all(&runtime_dir).map_err(|err| {
        ProtocolError::materialization_failed(&runtime_dir.display().to_string(), &err.to_string())
    })?;
    let snapshot_path = runtime_dir.join(SNAPSHOT_FILE);
    write_snapshot(&snapshot_path, snapshot, request_id.is_some()).map_err(|err| {
        ProtocolError::materialization_failed(
            &snapshot_path.display().to_string(),
            &err.to_string(),
        )
    })?;

    Ok(DegradedMaterializeResult {
        snapshot_path: snapshot_path.display().to_string(),
        version: BUNDLE_VERSION.to_owned(),
    })
}

fn materialize_local(
    paths: &AssistantPaths,
    snapshot: &str,
    request_id: Option<&str>,
) -> Result<AssistantMaterializeResult, AssistantError> {
    let version_hash = materialized_version_hash();
    let concepts: Vec<protocol::ConceptMeta> = bundle_index()
        .map_err(|err| {
            ProtocolError::materialization_failed("assistant bundle index", &err.to_string())
        })?
        .into_iter()
        .map(Into::into)
        .collect();
    let bundle_path =
        materialize_bundle(paths.cache_dir.clone(), &version_hash).map_err(|err| {
            ProtocolError::materialization_failed(
                &paths.assistant_bundle_cache_dir().display().to_string(),
                &err.to_string(),
            )
        })?;
    let runtime_dir = assistant_runtime_dir(paths, request_id)?;
    create_private_dir_all(&runtime_dir).map_err(|err| {
        ProtocolError::materialization_failed(&runtime_dir.display().to_string(), &err.to_string())
    })?;
    let snapshot_path = runtime_dir.join(SNAPSHOT_FILE);
    write_snapshot(&snapshot_path, snapshot, request_id.is_some()).map_err(|err| {
        ProtocolError::materialization_failed(
            &snapshot_path.display().to_string(),
            &err.to_string(),
        )
    })?;

    Ok(AssistantMaterializeResult {
        bundle_path: bundle_path.display().to_string(),
        snapshot_path: snapshot_path.display().to_string(),
        version: BUNDLE_VERSION.to_owned(),
        content_hash: bundle_content_hash().to_owned(),
        concepts,
    })
}

async fn materialize_remote(
    client: &mut Client,
    snapshot: &str,
) -> Result<AssistantMaterializeResult, AssistantError> {
    let params = AssistantMaterializeParams {
        snapshot: snapshot.to_owned(),
    };
    let result = client
        .call::<method::AssistantMaterialize>(params)
        .await
        .map_err(map_assistant_method_error)?;
    assert_bundle_matches(&result)?;
    Ok(result)
}

/// Writes the launch snapshot to `path`.
///
/// A launch under a request id owns its directory: the first launch writes the
/// snapshot and a repeat keeps it, so a repeat that the daemon refuses (another
/// request under the same id) cannot change the file a running session reads.
fn write_snapshot(path: &Path, snapshot: &str, keep_existing: bool) -> std::io::Result<()> {
    if !keep_existing {
        return std::fs::write(path, snapshot);
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => std::io::Write::write_all(&mut file, snapshot.as_bytes()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Creates `dir` and every missing ancestor owner-private. The runtime root
/// is `XDG_RUNTIME_DIR/pohunek`, which the daemon refuses unless it is 0700.
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Mode of every directory a launch creates above its runtime snapshot.
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;

/// The runtime directory of one local launch: unique per launch, or, under a
/// request id, a function of it, so a launch repeated under the id renders the
/// same snapshot path into its prompt.
fn assistant_runtime_dir(
    paths: &AssistantPaths,
    request_id: Option<&str>,
) -> Result<PathBuf, AssistantError> {
    let version_hash = materialized_version_hash();
    let launch_id = match request_id {
        Some(id) => assistant_keyed_launch_id(&version_hash, id),
        None => assistant_launch_id(&version_hash),
    };
    paths.assistant_runtime_dir(&launch_id).ok_or_else(|| {
        ProtocolError::materialization_failed("assistant runtime", "invalid launch id").into()
    })
}

fn assert_bundle_matches(result: &AssistantMaterializeResult) -> Result<(), AssistantError> {
    let expected_hash = bundle_content_hash();
    if result.version == BUNDLE_VERSION && result.content_hash == expected_hash {
        Ok(())
    } else {
        Err(ProtocolError::assistant_bundle_mismatch(
            BUNDLE_VERSION,
            expected_hash,
            &result.version,
            &result.content_hash,
        )
        .into())
    }
}

fn map_assistant_method_error(err: pohunek_client::ClientError) -> pohunek_client::ClientError {
    match err {
        pohunek_client::ClientError::Protocol(source) if source.code == "method_not_found" => {
            pohunek_client::ClientError::Protocol(ProtocolError::assistant_method_unsupported(
                method::ASSISTANT_MATERIALIZE,
            ))
        }
        pohunek_client::ClientError::RemoteProtocol { host, source }
            if source.code == "method_not_found" =>
        {
            pohunek_client::ClientError::RemoteProtocol {
                host,
                source: ProtocolError::assistant_method_unsupported(method::ASSISTANT_MATERIALIZE),
            }
        }
        other => other,
    }
}

fn preflight_read_access(
    bundle_path: &str,
    snapshot_path: &str,
    remote: bool,
) -> Result<(), AssistantError> {
    if remote {
        return Ok(());
    }

    let bundle_index = Path::new(bundle_path).join("index.md");
    check_readable(
        &bundle_index,
        "materialized knowledge bundle is not readable",
    )?;
    check_readable(
        Path::new(snapshot_path),
        "materialized snapshot is not readable",
    )?;
    Ok(())
}

fn preflight_snapshot_readable(snapshot_path: &str) -> Result<(), AssistantError> {
    check_readable(
        Path::new(snapshot_path),
        "degraded snapshot is not readable",
    )
}

fn check_readable(path: &Path, constraint: &str) -> Result<(), AssistantError> {
    match std::fs::File::open(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(ProtocolError::agent_cannot_read_bundle(
            &path.display().to_string(),
            &format!("{constraint}: {err}"),
        )
        .into()),
    }
}

#[derive(Debug, Serialize)]
struct Snapshot {
    assistant: SnapshotAssistant,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SnapshotAssistant {
    intent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_request: Option<String>,
    selected_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_project: Option<String>,
    selected_agent: String,
    auto_started_daemon: bool,
    knowledge_bundle_version: String,
    snapshot_collected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotOrientation {
    project: String,
    agent: String,
}

fn collect_snapshot(
    config: &HostConfig,
    _client: &mut Client,
    params: &LaunchParams,
    selected_agent: &str,
) -> (String, SnapshotOrientation) {
    let snapshot = Snapshot {
        assistant: SnapshotAssistant {
            intent: params.intent.as_str().to_owned(),
            user_request: params.request.clone(),
            selected_host: host_label(config),
            selected_project: params.project.clone(),
            selected_agent: selected_agent.to_owned(),
            auto_started_daemon: params.auto_started_daemon,
            knowledge_bundle_version: BUNDLE_VERSION.to_owned(),
            snapshot_collected: true,
        },
        warnings: Vec::new(),
    };
    let orientation = SnapshotOrientation {
        project: params
            .project
            .clone()
            .unwrap_or_else(|| "(none)".to_owned()),
        agent: selected_agent.to_owned(),
    };
    (serialize_snapshot(&snapshot), orientation)
}

fn skipped_snapshot(
    config: &HostConfig,
    params: &LaunchParams,
    selected_agent: &str,
) -> (String, SnapshotOrientation) {
    let snapshot = Snapshot {
        assistant: SnapshotAssistant {
            intent: params.intent.as_str().to_owned(),
            user_request: params.request.clone(),
            selected_host: host_label(config),
            selected_project: params.project.clone(),
            selected_agent: selected_agent.to_owned(),
            auto_started_daemon: params.auto_started_daemon,
            knowledge_bundle_version: BUNDLE_VERSION.to_owned(),
            snapshot_collected: false,
        },
        warnings: vec!["snapshot collection skipped (--no-snapshot)".to_owned()],
    };
    let orientation = SnapshotOrientation {
        project: params
            .project
            .clone()
            .unwrap_or_else(|| "(none)".to_owned()),
        agent: selected_agent.to_owned(),
    };
    (serialize_snapshot(&snapshot), orientation)
}

fn serialize_snapshot(snapshot: &Snapshot) -> String {
    serde_json::to_string_pretty(snapshot)
        .unwrap_or_else(|_| "{\"warnings\":[\"snapshot serialization failed\"]}".to_owned())
}

struct ComposeParams<'a> {
    contract: &'a knowledge::PromptContract,
    intent: Intent,
    request: Option<&'a str>,
    concepts: &'a [ConceptMeta],
    bundle_path: &'a str,
    snapshot_path: &'a str,
    orientation: &'a SnapshotOrientation,
    version: &'a str,
}

struct ComposeDegradedParams<'a> {
    contract: &'a knowledge::PromptContract,
    intent: Intent,
    request: Option<&'a str>,
    snapshot_path: &'a str,
    orientation: &'a SnapshotOrientation,
    version: &'a str,
    bundle_version_note: &'a str,
}

/// The prompt contract as embedded in this binary.
fn embedded_contract() -> Result<knowledge::PromptContract, AssistantError> {
    let text = knowledge::embedded_bundle()
        .get_text(knowledge::PROMPT_CONTRACT_PATH)
        .ok_or_else(|| contract_error("the embedded bundle has no prompt contract"))?;
    knowledge::PromptContract::parse(text).map_err(|error| contract_error(&error.to_string()))
}

fn contract_error(reason: &str) -> AssistantError {
    ProtocolError::materialization_failed("assistant prompt contract", reason).into()
}

fn write_contract_safety(prompt: &mut String, contract: &knowledge::PromptContract) {
    let _ = writeln!(prompt, "## Safety");
    for rule in &contract.safety {
        let _ = writeln!(prompt, "- {rule}");
    }
    let _ = writeln!(prompt);
}

fn compose(params: &ComposeParams<'_>) -> String {
    let mut prompt = String::with_capacity(2048);

    let _ = writeln!(prompt, "# Pohunek Assistant\n");
    let _ = writeln!(prompt, "## Mission");
    let _ = writeln!(
        prompt,
        "You are the universal assistant for configuring, updating, troubleshooting, and \
         explaining pohunek (version {}).\n",
        params.version
    );
    write_contract_safety(&mut prompt, params.contract);
    let _ = writeln!(prompt, "## User Intent");
    let _ = writeln!(prompt, "intent: {}", params.intent.as_str());
    let _ = writeln!(
        prompt,
        "request: {}\n",
        params.request.unwrap_or("(none — orient and offer help)")
    );
    let _ = writeln!(prompt, "## Your Knowledge Base");
    let _ = writeln!(prompt, "Directory: {}\n", params.bundle_path);
    let _ = writeln!(prompt, "## Navigation");
    for (number, step) in params.contract.navigation.iter().enumerate() {
        let _ = writeln!(prompt, "{}. {step}", number + 1);
    }
    let _ = writeln!(prompt);
    let _ = writeln!(
        prompt,
        "## Relevant Concepts (intent: {})",
        params.intent.as_str()
    );
    write_toc(&mut prompt, params.intent, params.concepts);
    let _ = writeln!(prompt);
    write_snapshot_section(&mut prompt, params.orientation, params.snapshot_path);
    let _ = writeln!(prompt, "## Source Map");
    let _ = writeln!(prompt, "{}/assistant/source-map.md\n", params.bundle_path);
    let _ = write!(
        prompt,
        "Follow the navigation steps, take the next concrete action, and verify changes."
    );

    prompt
}

fn compose_degraded(params: &ComposeDegradedParams<'_>) -> String {
    let mut prompt = String::with_capacity(1024);

    let _ = writeln!(prompt, "# Pohunek Assistant (degraded)\n");
    let _ = writeln!(prompt, "## Knowledge Status");
    let _ = writeln!(
        prompt,
        "knowledge: degraded for version {} (bundle version note: {}).\n",
        params.version, params.bundle_version_note
    );
    write_contract_safety(&mut prompt, params.contract);
    let _ = writeln!(prompt, "## User Intent");
    let _ = writeln!(prompt, "intent: {}", params.intent.as_str());
    let _ = writeln!(
        prompt,
        "request: {}\n",
        params.request.unwrap_or("(none — orient and offer help)")
    );
    write_snapshot_section(&mut prompt, params.orientation, params.snapshot_path);
    let _ = write!(
        prompt,
        "Use the launch snapshot and source tree access. Verify changes before claiming success."
    );

    prompt
}

fn write_snapshot_section(
    prompt: &mut String,
    orientation: &SnapshotOrientation,
    snapshot_path: &str,
) {
    let _ = writeln!(prompt, "## Launch Snapshot");
    let _ = writeln!(
        prompt,
        "Orientation: project={}, agent={}",
        orientation.project, orientation.agent
    );
    let _ = writeln!(prompt, "Full file: {snapshot_path}\n");
}

fn write_toc(prompt: &mut String, intent: Intent, concepts: &[ConceptMeta]) {
    let wanted = intent.as_concept_intent();
    let mut listed = 0usize;
    for concept in concepts {
        if concept
            .intents
            .as_ref()
            .is_some_and(|intents| intents.contains(&wanted))
        {
            let _ = writeln!(prompt, "- {} — {}", concept.id, concept.description);
            listed += 1;
        }
    }

    if listed == 0 {
        for concept in concepts {
            let _ = writeln!(prompt, "- {} — {}", concept.id, concept.description);
        }
    }
}

fn path_error(err: pohunek_paths::PathError) -> AssistantError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => AssistantError::MissingEnv { var },
        source => AssistantError::Paths { source },
    }
}
