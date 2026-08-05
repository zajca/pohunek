//! Shared assistant launch primitives.
//!
//! The module contains client-side assistant orchestration used by both the CLI
//! and native GUI. The daemon still sees ordinary protocol requests.

// Rust guideline compliant 2026-07-01

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use knowledge::{
    assistant_launch_id, bundle_content_hash, bundle_index, materialize as materialize_bundle,
    materialized_version_hash, BUNDLE_VERSION,
};
use pohunek_client::Client;
use protocol::{
    method, AgentKind, AssistantMaterializeParams, AssistantMaterializeResult, ConceptIntent,
    ConceptMeta, HostCapabilities, ProtocolError, SessionInfo, SessionNewParams,
};
use serde::Serialize;

use crate::connection::connect_client;
use crate::{
    runtime_is_assistant_capable, runtime_is_launchable, ConnectionOptions, CoreError, HostConfig,
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
    /// Returns [`CoreError::MissingEnv`] when the required XDG or `HOME`
    /// environment variables are absent.
    pub fn resolve() -> Result<Self, CoreError> {
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
        pohunek_paths::valid_runtime_id(launch_id).map(|id| {
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
    pub cols: u16,
    pub rows: u16,
    pub no_snapshot: bool,
    pub degraded: bool,
    pub auto_started_daemon: bool,
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
/// Explicit Hermes choices must be available with positive support confirmation.
/// Other explicit names remain daemon-authoritative for profile resolution.
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
/// Returns [`CoreError`] when target validation, host inspection, snapshot
/// materialization, read preflight, or prompt construction fails.
pub async fn prepare_with_options(
    config: &HostConfig,
    paths: &AssistantPaths,
    params: LaunchParams,
    options: ConnectionOptions,
) -> Result<PreparedLaunch, CoreError> {
    validate_target(config, &params)?;
    let mut client = connect_client(config, options).await?;
    let capabilities = fetch_capabilities(&mut client).await?;
    let selection = select_agent(&capabilities, params.agent.as_deref())?;
    let (snapshot, orientation) = if params.no_snapshot {
        skipped_snapshot(config, &params, &selection.name)
    } else {
        collect_snapshot(config, &mut client, &params, &selection.name)
    };

    let knowledge = if params.degraded {
        let degraded = materialize_degraded(paths, &snapshot)?;
        preflight_snapshot_readable(&degraded.snapshot_path)?;
        PreparedKnowledge::Degraded(degraded)
    } else if is_remote(config) {
        PreparedKnowledge::Materialized(materialize_remote(&mut client, &snapshot).await?)
    } else {
        let materialized = materialize_local(paths, &snapshot)?;
        preflight_read_access(
            &materialized.bundle_path,
            &materialized.snapshot_path,
            false,
        )?;
        PreparedKnowledge::Materialized(materialized)
    };

    let prompt = match &knowledge {
        PreparedKnowledge::Materialized(materialized) => compose(&ComposeParams {
            intent: params.intent,
            request: params.request.as_deref(),
            concepts: &materialized.concepts,
            bundle_path: &materialized.bundle_path,
            snapshot_path: &materialized.snapshot_path,
            orientation: &orientation,
            version: &materialized.version,
        }),
        PreparedKnowledge::Degraded(degraded) => compose_degraded(&ComposeDegradedParams {
            intent: params.intent,
            request: params.request.as_deref(),
            snapshot_path: &degraded.snapshot_path,
            orientation: &orientation,
            version: &degraded.version,
            bundle_version_note: &degraded.version,
        }),
    };

    let session_params = SessionNewParams {
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
        metadata: std::collections::BTreeMap::new(),
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
/// Returns [`CoreError`] when `session.new` fails.
pub async fn start_prepared_with_options(
    config: &HostConfig,
    prepared: PreparedLaunch,
    options: ConnectionOptions,
) -> Result<LaunchResult, CoreError> {
    let mut client = connect_client(config, options).await?;
    let result = client
        .call::<method::SessionNew>(prepared.session_params)
        .await?;

    Ok(LaunchResult {
        session: result.session,
        applied_input: result.applied_input,
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

/// Prepare and start an assistant session.
///
/// # Errors
///
/// Returns [`CoreError`] when preparation or `session.new` fails.
pub async fn launch_with_options(
    config: &HostConfig,
    paths: &AssistantPaths,
    params: LaunchParams,
    options: ConnectionOptions,
) -> Result<LaunchResult, CoreError> {
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
    let runtime = capabilities
        .runtimes
        .iter()
        .find(|runtime| runtime.agent == agent);
    if runtime
        .is_some_and(|runtime| matches!(runtime.agent_base.as_ref(), Some(AgentKind::Unknown(_))))
    {
        return false;
    }

    if agent == "hermes"
        || runtime.is_some_and(|runtime| runtime.agent_base.as_ref() == Some(&AgentKind::Hermes))
    {
        return runtime.is_some_and(runtime_is_launchable);
    }

    true
}

fn validate_target(config: &HostConfig, params: &LaunchParams) -> Result<(), CoreError> {
    if !is_remote(config) {
        return Ok(());
    }
    let host = host_label(config);
    if params.degraded {
        return Err(CoreError::RemoteAssistantDegradedUnsupported { host });
    }
    if params.project.is_none() && params.repo.is_none() {
        return Err(CoreError::RemoteAssistantTargetRequired { host });
    }
    Ok(())
}

fn is_remote(config: &HostConfig) -> bool {
    !matches!(config.transport, HostTransport::Local { .. })
}

fn host_label(config: &HostConfig) -> String {
    match &config.transport {
        HostTransport::Local { .. } => "local".to_owned(),
        HostTransport::Remote { host, .. } => host.clone(),
        HostTransport::Tcp { .. } => config.id.as_str().to_owned(),
    }
}

async fn fetch_capabilities(client: &mut Client) -> Result<HostCapabilities, CoreError> {
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
) -> Result<DegradedMaterializeResult, CoreError> {
    let runtime_dir = assistant_runtime_dir(paths)?;
    std::fs::create_dir_all(&runtime_dir).map_err(|err| {
        ProtocolError::materialization_failed(&runtime_dir.display().to_string(), &err.to_string())
    })?;
    let snapshot_path = runtime_dir.join(SNAPSHOT_FILE);
    std::fs::write(&snapshot_path, snapshot).map_err(|err| {
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
) -> Result<AssistantMaterializeResult, CoreError> {
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
    let runtime_dir = assistant_runtime_dir(paths)?;
    std::fs::create_dir_all(&runtime_dir).map_err(|err| {
        ProtocolError::materialization_failed(&runtime_dir.display().to_string(), &err.to_string())
    })?;
    let snapshot_path = runtime_dir.join(SNAPSHOT_FILE);
    std::fs::write(&snapshot_path, snapshot).map_err(|err| {
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
) -> Result<AssistantMaterializeResult, CoreError> {
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

fn assistant_runtime_dir(paths: &AssistantPaths) -> Result<PathBuf, CoreError> {
    let version_hash = materialized_version_hash();
    let launch_id = assistant_launch_id(&version_hash);
    paths.assistant_runtime_dir(&launch_id).ok_or_else(|| {
        ProtocolError::materialization_failed("assistant runtime", "invalid launch id").into()
    })
}

fn assert_bundle_matches(result: &AssistantMaterializeResult) -> Result<(), CoreError> {
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
) -> Result<(), CoreError> {
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

fn preflight_snapshot_readable(snapshot_path: &str) -> Result<(), CoreError> {
    check_readable(
        Path::new(snapshot_path),
        "degraded snapshot is not readable",
    )
}

fn check_readable(path: &Path, constraint: &str) -> Result<(), CoreError> {
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
    daemon: String,
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
        daemon: "unknown".to_owned(),
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
        daemon: "unknown".to_owned(),
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
    intent: Intent,
    request: Option<&'a str>,
    concepts: &'a [ConceptMeta],
    bundle_path: &'a str,
    snapshot_path: &'a str,
    orientation: &'a SnapshotOrientation,
    version: &'a str,
}

struct ComposeDegradedParams<'a> {
    intent: Intent,
    request: Option<&'a str>,
    snapshot_path: &'a str,
    orientation: &'a SnapshotOrientation,
    version: &'a str,
    bundle_version_note: &'a str,
}

const INLINE_SAFETY: &str = "\
- Never print, store, or infer secret values.
- Treat agent profile [env] values as secret-bearing.
- Explain config edits before applying them; preserve user edits unless asked to overwrite.
- Hooks are executable code: creation or modification requires explicit per-file confirmation.
- Prefer structured --json inspection commands.
- Verify changes after applying them before claiming success.";

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
    let _ = writeln!(prompt, "## Safety");
    let _ = writeln!(prompt, "{INLINE_SAFETY}\n");
    let _ = writeln!(prompt, "## User Intent");
    let _ = writeln!(prompt, "intent: {}", params.intent.as_str());
    let _ = writeln!(
        prompt,
        "request: {}\n",
        params.request.unwrap_or("(none — orient and offer help)")
    );
    let _ = writeln!(prompt, "## Your Knowledge Base");
    let _ = writeln!(prompt, "Directory: {}", params.bundle_path);
    let _ = writeln!(
        prompt,
        "Start at index.md and read only relevant concepts.\n"
    );
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
        "Read the snapshot, open relevant concepts, take the next concrete action, and verify changes."
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
    let _ = writeln!(prompt, "## Safety");
    let _ = writeln!(prompt, "{INLINE_SAFETY}\n");
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
        "Use the snapshot and source tree access. Verify changes before claiming success."
    );

    prompt
}

fn write_snapshot_section(
    prompt: &mut String,
    orientation: &SnapshotOrientation,
    snapshot_path: &str,
) {
    let _ = writeln!(prompt, "## Live Snapshot");
    let _ = writeln!(
        prompt,
        "Orientation: daemon={}, project={}, agent={}",
        orientation.daemon, orientation.project, orientation.agent
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

fn path_error(err: pohunek_paths::PathError) -> CoreError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => CoreError::MissingEnv { var },
    }
}
