//! Per-agent hook installation.
//!
//! Ported from herdr (`src/integration/mod.rs` `install_claude`/`install_codex`
//! and `assets/{claude,codex}/herdr-agent-state.sh`), rewritten to emit *our*
//! handshake env names and *our* active-agent/native-id callback methods. The
//! hook reports nested active-agent identity for the owning session and captures
//! the launch agent's native session id for direct-session resume; live activity
//! still comes from the detector unless a hook has reliable activity evidence.
//!
//! Install merges into the agent's own config format idempotently and never
//! clobbers unrelated user hooks: only exact command strings written by this
//! installer are stripped before managed hooks are (re-)added.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use protocol::{
    AgentKind, ErrorClass, IntegrationAgentStatus, IntegrationInstallReport,
    IntegrationInstallResult, IntegrationInstallState, IntegrationStatusParams,
    IntegrationStatusResult, ProtocolError, EXPECTED_INTEGRATION_VERSION,
};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::{value, DocumentMut, Item, Table};

// The agent-handshake env var names are defined once in `protocol` (the shared
// contract crate) so the daemon (which injects them), the installed hook (which
// reads them), and the CLI (which reads `ENV_SESSION_ID` for the
// self-feeding-attach guard) cannot drift. Re-exported here so existing daemon
// call sites and tests keep referring to `integration::ENV_*` unchanged.
pub use protocol::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};

/// Installed active-agent state hook script file name (shared by both agents).
const STATE_HOOK_INSTALL_NAME: &str = "pohunek-agent-state.sh";
/// Installed notification hook script file name (shared by both agents).
const NOTIFY_HOOK_INSTALL_NAME: &str = "pohunek-agent-notify.sh";
/// Marker prefix used to identify the installed managed asset version.
const INTEGRATION_VERSION_PREFIX: &str = "# POHUNEK_INTEGRATION_VERSION=";
/// The Claude hook script, embedded at compile time.
const CLAUDE_HOOK_ASSET: &str = include_str!("assets/claude/pohunek-agent-state.sh");
/// The Claude notification hook script, embedded at compile time.
const CLAUDE_NOTIFY_HOOK_ASSET: &str = include_str!("assets/claude/pohunek-agent-notify.sh");
/// The Codex hook script, embedded at compile time.
const CODEX_HOOK_ASSET: &str = include_str!("assets/codex/pohunek-agent-state.sh");
/// The Codex notification hook script, embedded at compile time.
const CODEX_NOTIFY_HOOK_ASSET: &str = include_str!("assets/codex/pohunek-agent-notify.sh");
/// Per-hook timeout (seconds) recorded in the agent's hook config.
const HOOK_TIMEOUT_SECS: u64 = 10;
/// Action argument passed to the hook script for the `SessionStart` event.
const HOOK_ACTION: &str = "session";
/// Action argument passed to the hook script for the `SessionEnd` event.
const HOOK_RELEASE_ACTION: &str = "release";
/// `SessionStart` event name in the agents' hook config.
const SESSION_START_EVENT: &str = "SessionStart";
/// `SessionEnd` event name in Claude's hook config.
const SESSION_END_EVENT: &str = "SessionEnd";
/// Codex lifecycle event fired before provider approval prompts.
const CODEX_PERMISSION_REQUEST_EVENT: &str = "PermissionRequest";
/// Codex lifecycle event fired when a turn completes.
const CODEX_STOP_EVENT: &str = "Stop";
/// Claude event family for interactive notifications.
const CLAUDE_NOTIFICATION_EVENT: &str = "Notification";
/// Claude event fired when a turn completes.
const CLAUDE_STOP_EVENT: &str = "Stop";
/// Claude event fired when a stop hook reports failure.
const CLAUDE_STOP_FAILURE_EVENT: &str = "StopFailure";
/// Codex trust identity name for `SessionStart`.
const CODEX_SESSION_START_TRUST_EVENT: &str = "session_start";
/// Codex trust identity name for `PermissionRequest`.
const CODEX_PERMISSION_REQUEST_TRUST_EVENT: &str = "permission_request";
/// Codex trust identity name for `Stop`.
const CODEX_STOP_TRUST_EVENT: &str = "stop";
/// Action argument passed to notification hook scripts for approval prompts.
const PERMISSION_REQUEST_ACTION: &str = "permission_request";
/// Action argument passed to notification hook scripts for notifications.
const NOTIFICATION_ACTION: &str = "notification";
/// Action argument passed to notification hook scripts for turn completion.
const STOP_ACTION: &str = "stop";
/// Action argument passed to notification hook scripts for stop failures.
const STOP_FAILURE_ACTION: &str = "stop_failure";
/// Claude `Notification` matchers that map to durable notifications.
const CLAUDE_NOTIFICATION_MATCHERS: &[&str] = &[
    "permission_prompt",
    "elicitation_dialog",
    "auth_success",
    "elicitation_complete",
    "elicitation_response",
];

/// Exact Unix mode written for Pohunek-managed hook executables.
///
/// Group or other write access would let another local principal alter code
/// executed by the owner. Other modes are drift from the installer contract.
#[cfg(unix)]
const MANAGED_HOOK_MODE: u32 = 0o755;

/// Env var overriding Claude's config dir (else `~/.claude`).
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Env var overriding Codex's config dir (else `~/.codex`).
const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Files the installer wrote for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// Absolute path of the installed hook script.
    pub hook_path: PathBuf,
    /// Config files created or merged into, in the order touched.
    pub config_paths: Vec<PathBuf>,
}

/// Install the `SessionStart` hook for the selected agent(s).
///
/// `Some(agent)` installs that agent only and fails fast if its config dir is
/// absent. `None` installs the hook for every supported agent whose config dir
/// exists, and errors only if none are present.
///
/// # Errors
///
/// `agent_not_installable` for `AgentKind::Shell`, `agent_config_dir_missing`
/// when a requested (or, for `None`, every) agent config dir is absent, or any
/// underlying I/O / settings error.
pub fn install(agent: Option<AgentKind>) -> Result<IntegrationInstallResult, ProtocolError> {
    let installed = match agent {
        Some(AgentKind::Claude) => {
            vec![report(
                AgentKind::Claude,
                &install_claude(&claude_config_dir()?)?,
            )]
        }
        Some(AgentKind::Codex) => {
            vec![report(
                AgentKind::Codex,
                &install_codex(&codex_config_dir()?)?,
            )]
        }
        Some(AgentKind::Shell) => {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_not_installable",
                "shell sessions have no hook integration",
                None,
            ));
        }
        Some(AgentKind::Hermes) => {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_not_installable",
                "Hermes integration is not available in this milestone",
                None,
            ));
        }
        Some(AgentKind::Unknown(agent)) => {
            return Err(ProtocolError::agent_kind_unsupported(&agent));
        }
        None => install_all_present()?,
    };
    Ok(IntegrationInstallResult { installed })
}

/// Inspect managed Codex and Claude hook files without writing anything.
///
/// Unsupported agents are rejected so callers cannot mistake an empty report
/// for "nothing is installed". Missing config directories are reported as
/// unavailable rather than treated as errors.
///
/// # Errors
///
/// Returns [`ProtocolError`] for unsupported agents or when the agent config
/// directory cannot be resolved.
pub fn status(params: IntegrationStatusParams) -> Result<IntegrationStatusResult, ProtocolError> {
    let agents = match params.agent {
        Some(AgentKind::Claude) => vec![reported_agent_status(StatusAgent::Claude)],
        Some(AgentKind::Codex) => vec![reported_agent_status(StatusAgent::Codex)],
        Some(AgentKind::Shell) => return Err(status_unsupported(&AgentKind::Shell)),
        Some(AgentKind::Hermes) => return Err(status_unsupported(&AgentKind::Hermes)),
        Some(AgentKind::Unknown(value)) => {
            return Err(ProtocolError::agent_kind_unsupported(&value));
        }
        None => vec![
            reported_agent_status(StatusAgent::Claude),
            reported_agent_status(StatusAgent::Codex),
        ],
    };
    Ok(IntegrationStatusResult { agents })
}

/// Degrade one supported agent's config-resolution failure into a warning.
fn reported_agent_status(agent: StatusAgent) -> IntegrationAgentStatus {
    match agent_status(agent) {
        Ok(report) => report,
        Err(error) => IntegrationAgentStatus {
            agent: agent.kind(),
            available: false,
            expected_asset_paths: Vec::new(),
            present_asset_paths: Vec::new(),
            registration_paths: Vec::new(),
            installed_version: None,
            expected_version: EXPECTED_INTEGRATION_VERSION,
            state: IntegrationInstallState::Outdated,
            warnings: vec![format!(
                "agent config directory could not be resolved ({})",
                error.code
            )],
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusAgent {
    Claude,
    Codex,
}

impl StatusAgent {
    fn kind(self) -> AgentKind {
        match self {
            Self::Claude => AgentKind::Claude,
            Self::Codex => AgentKind::Codex,
        }
    }
}

fn status_unsupported(agent: &AgentKind) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_not_installable",
        format!("{} has no daemon-managed hook integration", agent.as_wire()),
        None,
    )
}

/// Build one supported agent's read-only status report.
fn agent_status(agent: StatusAgent) -> Result<IntegrationAgentStatus, ProtocolError> {
    match agent {
        StatusAgent::Claude => Ok(status_at(
            StatusAgent::Claude,
            &claude_config_dir()?,
            CLAUDE_HOOK_ASSET,
            CLAUDE_NOTIFY_HOOK_ASSET,
        )),
        StatusAgent::Codex => Ok(status_at(
            StatusAgent::Codex,
            &codex_config_dir()?,
            CODEX_HOOK_ASSET,
            CODEX_NOTIFY_HOOK_ASSET,
        )),
    }
}

fn status_at(
    agent: StatusAgent,
    config_dir: &Path,
    state_asset: &'static str,
    notify_asset: &'static str,
) -> IntegrationAgentStatus {
    let state_path = managed_hook_path(config_dir, agent, STATE_HOOK_INSTALL_NAME);
    let notify_path = managed_hook_path(config_dir, agent, NOTIFY_HOOK_INSTALL_NAME);
    let registration_paths = registration_paths(config_dir, agent);
    let expected_asset_paths = vec![path_text(&state_path), path_text(&notify_path)];
    let registration_path_strings = registration_paths
        .iter()
        .map(|path| path_text(path))
        .collect();

    let available = match fs::metadata(config_dir) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_metadata) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_error) => false,
    };
    if !available {
        let warning = match fs::metadata(config_dir) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                "agent config directory could not be inspected"
            }
            Ok(_metadata) => "agent config path is not a directory",
            _ => "agent config directory does not exist",
        };
        return IntegrationAgentStatus {
            agent: agent.kind(),
            available: false,
            expected_asset_paths,
            present_asset_paths: Vec::new(),
            registration_paths: registration_path_strings,
            installed_version: None,
            expected_version: EXPECTED_INTEGRATION_VERSION,
            state: if warning == "agent config directory does not exist" {
                IntegrationInstallState::NotInstalled
            } else {
                IntegrationInstallState::Outdated
            },
            warnings: vec![warning.to_owned()],
        };
    }

    let mut warnings = Vec::new();
    let assets = [
        inspect_asset(
            agent,
            ManagedAssetKind::StateHook,
            state_path,
            state_asset,
            &mut warnings,
        ),
        inspect_asset(
            agent,
            ManagedAssetKind::NotificationHook,
            notify_path,
            notify_asset,
            &mut warnings,
        ),
    ];
    let (registration_footprint, registrations_current) = match agent {
        StatusAgent::Claude => {
            inspect_claude_registration(config_dir, &assets[0].path, &assets[1].path, &mut warnings)
        }
        StatusAgent::Codex => {
            inspect_codex_registration(config_dir, &assets[0].path, &assets[1].path, &mut warnings)
        }
    };
    let present_asset_paths = assets
        .iter()
        .filter(|asset| asset.present)
        .map(|asset| path_text(&asset.path))
        .collect();
    let installed_version = installed_version(&assets, &mut warnings);
    let footprint = registration_footprint || assets.iter().any(|asset| asset.footprint);
    let state = if !footprint {
        IntegrationInstallState::NotInstalled
    } else if registrations_current && assets.iter().all(|asset| asset.current) {
        IntegrationInstallState::Current
    } else {
        IntegrationInstallState::Outdated
    };

    IntegrationAgentStatus {
        agent: agent.kind(),
        available,
        expected_asset_paths,
        present_asset_paths,
        registration_paths: registration_path_strings,
        installed_version,
        expected_version: EXPECTED_INTEGRATION_VERSION,
        state,
        warnings,
    }
}

/// Resolve the platform-specific managed hook path for an agent.
fn managed_hook_path(config_dir: &Path, agent: StatusAgent, file_name: &str) -> PathBuf {
    match agent {
        StatusAgent::Claude => config_dir.join("hooks").join(file_name),
        StatusAgent::Codex => config_dir.join(file_name),
    }
}

/// Parse the first exact integration-version marker in a hook asset.
fn parse_integration_version(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        line.trim()
            .strip_prefix(INTEGRATION_VERSION_PREFIX)
            .and_then(|version| version.parse::<u32>().ok())
    })
}

#[derive(Debug)]
struct AssetStatus {
    path: PathBuf,
    content: Option<String>,
    present: bool,
    footprint: bool,
    current: bool,
}

/// Independently classifies each read-only managed hook asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedAssetKind {
    StateHook,
    NotificationHook,
}

impl ManagedAssetKind {
    fn description(self) -> &'static str {
        match self {
            Self::StateHook => "state hook",
            Self::NotificationHook => "notification hook",
        }
    }
}

fn inspect_asset(
    agent: StatusAgent,
    kind: ManagedAssetKind,
    path: PathBuf,
    expected: &'static str,
    warnings: &mut Vec<String>,
) -> AssetStatus {
    let description = kind.description();
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warnings.push(format!("managed {description} is missing"));
            return AssetStatus {
                path,
                content: None,
                present: false,
                footprint: false,
                current: false,
            };
        }
        Err(_error) => {
            warnings.push(format!("managed {description} metadata could not be read"));
            return AssetStatus {
                path,
                content: None,
                present: false,
                footprint: true,
                current: false,
            };
        }
    };
    if !metadata.file_type().is_file() {
        warnings.push(format!("managed {description} is not a regular file"));
        return AssetStatus {
            path,
            content: None,
            present: true,
            footprint: true,
            current: false,
        };
    }
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_error) => {
            warnings.push(format!("managed {description} could not be read"));
            return AssetStatus {
                path,
                content: None,
                present: true,
                footprint: true,
                current: false,
            };
        }
    };
    let version = parse_integration_version(&content);
    let content_current = content == expected;
    if !content_current {
        match version {
            Some(version) if version != EXPECTED_INTEGRATION_VERSION => warnings.push(format!(
                "managed {description} version {version} does not match expected {EXPECTED_INTEGRATION_VERSION}"
            )),
            Some(_version) => warnings.push(format!(
                "managed {description} content differs from the embedded asset"
            )),
            None => warnings.push(format!(
                "managed {description} version marker is missing or invalid"
            )),
        }
    }
    let permissions_current = asset_permissions_current(agent, kind, &metadata, warnings);
    AssetStatus {
        path,
        content: Some(content),
        present: true,
        footprint: true,
        current: content_current && permissions_current,
    }
}

#[cfg(unix)]
fn asset_permissions_current(
    agent: StatusAgent,
    kind: ManagedAssetKind,
    metadata: &fs::Metadata,
    warnings: &mut Vec<String>,
) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode == MANAGED_HOOK_MODE {
        return true;
    }

    let description = kind.description();
    let agent = agent.kind();
    if mode & 0o022 != 0 {
        warnings.push(format!(
            "managed {description} permissions are unsafe (mode {mode:04o}; group or other users can write it); run `pohunek integration install --agent {}` to restore mode {MANAGED_HOOK_MODE:04o}",
            agent.as_wire()
        ));
    } else {
        warnings.push(format!(
            "managed {description} permissions drifted (mode {mode:04o}; expected {MANAGED_HOOK_MODE:04o}); run `pohunek integration install --agent {}` to restore them",
            agent.as_wire()
        ));
    }
    false
}

#[cfg(not(unix))]
fn asset_permissions_current(
    _agent: StatusAgent,
    _kind: ManagedAssetKind,
    _metadata: &fs::Metadata,
    _warnings: &mut Vec<String>,
) -> bool {
    true
}

fn installed_version(assets: &[AssetStatus], warnings: &mut Vec<String>) -> Option<u32> {
    let mut versions = assets
        .iter()
        .filter_map(|asset| asset.content.as_deref())
        .filter_map(parse_integration_version)
        .collect::<Vec<_>>();
    versions.sort_unstable();
    versions.dedup();
    if versions.len() > 1 {
        warnings.push("managed asset version markers are inconsistent".to_owned());
        None
    } else {
        versions.first().copied()
    }
}

fn registration_paths(config_dir: &Path, agent: StatusAgent) -> Vec<PathBuf> {
    match agent {
        StatusAgent::Claude => vec![config_dir.join("settings.json")],
        StatusAgent::Codex => vec![
            config_dir.join("hooks.json"),
            config_dir.join("config.toml"),
        ],
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[derive(Debug)]
struct HookSpec {
    label: String,
    event: &'static str,
    command: String,
    matcher: Option<&'static str>,
    trust_event: Option<&'static str>,
}

#[derive(Debug)]
struct HookFileStatus {
    footprint: bool,
    current: bool,
    positions: Vec<Option<(usize, usize)>>,
}

enum ReadState {
    Missing,
    Unreadable,
    Content(String),
}

fn inspect_claude_registration(
    config_dir: &Path,
    state_path: &Path,
    notify_path: &Path,
    warnings: &mut Vec<String>,
) -> (bool, bool) {
    let settings_path = config_dir.join("settings.json");
    let mut specs = vec![
        HookSpec {
            label: "Claude SessionStart".to_owned(),
            event: SESSION_START_EVENT,
            command: hook_command(state_path, HOOK_ACTION),
            matcher: Some("*"),
            trust_event: None,
        },
        HookSpec {
            label: "Claude SessionEnd".to_owned(),
            event: SESSION_END_EVENT,
            command: hook_command(state_path, HOOK_RELEASE_ACTION),
            matcher: Some("*"),
            trust_event: None,
        },
    ];
    specs.extend(CLAUDE_NOTIFICATION_MATCHERS.iter().map(|matcher| HookSpec {
        label: format!("Claude Notification ({matcher})"),
        event: CLAUDE_NOTIFICATION_EVENT,
        command: hook_command_with_args(notify_path, &[NOTIFICATION_ACTION, matcher]),
        matcher: Some(matcher),
        trust_event: None,
    }));
    specs.extend([
        HookSpec {
            label: "Claude Stop".to_owned(),
            event: CLAUDE_STOP_EVENT,
            command: hook_command(notify_path, STOP_ACTION),
            matcher: Some("*"),
            trust_event: None,
        },
        HookSpec {
            label: "Claude StopFailure".to_owned(),
            event: CLAUDE_STOP_FAILURE_EVENT,
            command: hook_command(notify_path, STOP_FAILURE_ACTION),
            matcher: Some("*"),
            trust_event: None,
        },
    ]);
    let status = inspect_hook_file(
        StatusAgent::Claude,
        &settings_path,
        "Claude settings.json",
        &specs,
        warnings,
    );
    (status.footprint, status.current)
}

fn inspect_codex_registration(
    config_dir: &Path,
    state_path: &Path,
    notify_path: &Path,
    warnings: &mut Vec<String>,
) -> (bool, bool) {
    let hooks_path = config_dir.join("hooks.json");
    let specs = vec![
        HookSpec {
            label: "Codex SessionStart".to_owned(),
            event: SESSION_START_EVENT,
            command: hook_command(state_path, HOOK_ACTION),
            matcher: None,
            trust_event: Some(CODEX_SESSION_START_TRUST_EVENT),
        },
        HookSpec {
            label: "Codex PermissionRequest".to_owned(),
            event: CODEX_PERMISSION_REQUEST_EVENT,
            command: hook_command(notify_path, PERMISSION_REQUEST_ACTION),
            matcher: None,
            trust_event: Some(CODEX_PERMISSION_REQUEST_TRUST_EVENT),
        },
        HookSpec {
            label: "Codex Stop".to_owned(),
            event: CODEX_STOP_EVENT,
            command: hook_command(notify_path, STOP_ACTION),
            matcher: None,
            trust_event: Some(CODEX_STOP_TRUST_EVENT),
        },
    ];
    let hooks = inspect_hook_file(
        StatusAgent::Codex,
        &hooks_path,
        "Codex hooks.json",
        &specs,
        warnings,
    );
    let config = inspect_codex_config(
        &config_dir.join("config.toml"),
        &hooks_path,
        &specs,
        &hooks.positions,
        warnings,
    );
    (hooks.footprint || config.0, hooks.current && config.1)
}

fn inspect_hook_file(
    agent: StatusAgent,
    path: &Path,
    label: &str,
    specs: &[HookSpec],
    warnings: &mut Vec<String>,
) -> HookFileStatus {
    let content = match read_status_file(path, label, warnings) {
        ReadState::Missing => {
            return HookFileStatus {
                footprint: false,
                current: false,
                positions: vec![None; specs.len()],
            };
        }
        ReadState::Unreadable => {
            return HookFileStatus {
                footprint: true,
                current: false,
                positions: vec![None; specs.len()],
            };
        }
        ReadState::Content(content) => content,
    };
    let document: Value = match serde_json::from_str(&content) {
        Ok(document) => document,
        Err(_error) => {
            warnings.push(format!("{label} is malformed"));
            return HookFileStatus {
                footprint: true,
                current: false,
                positions: vec![None; specs.len()],
            };
        }
    };
    let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
        warnings.push(format!("{label} has no valid hooks object"));
        return HookFileStatus {
            footprint: document.get("hooks").is_some(),
            current: false,
            positions: vec![None; specs.len()],
        };
    };

    let mut footprint = false;
    let mut current = true;
    let mut positions = Vec::with_capacity(specs.len());
    for spec in specs {
        let references = hook_command_positions(hooks, spec.event, &spec.command);
        let reference_count = hook_command_count(hooks, &spec.command);
        footprint |= reference_count > 0;
        let exact = references
            .iter()
            .copied()
            .filter(|&(group_index, handler_index)| {
                hook_registration_matches(hooks, spec.event, group_index, handler_index, spec)
            })
            .collect::<Vec<_>>();
        let spec_current = reference_count == 1 && references.len() == 1 && exact.len() == 1;
        if !spec_current {
            let agent = agent.kind();
            if reference_count > references.len() {
                warnings.push(format!(
                    "managed {} registration also appears under an unexpected event; run `pohunek integration install --agent {}` to remove the duplicate",
                    spec.label,
                    agent.as_wire()
                ));
            } else {
                warnings.push(format!(
                    "managed {} registration is missing or modified; run `pohunek integration install --agent {}` to restore it",
                    spec.label,
                    agent.as_wire()
                ));
            }
        }
        current &= spec_current;
        positions.push(spec_current.then(|| exact[0]));
    }
    HookFileStatus {
        footprint,
        current,
        positions,
    }
}

fn hook_command_positions(
    hooks: &Map<String, Value>,
    event: &str,
    command: &str,
) -> Vec<(usize, usize)> {
    hooks
        .get(event)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(group_index, group)| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(move |(handler_index, handler)| {
                    (handler.get("command").and_then(Value::as_str) == Some(command))
                        .then_some((group_index, handler_index))
                })
        })
        .collect()
}

fn hook_command_count(hooks: &Map<String, Value>, command: &str) -> usize {
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|handler| handler.get("command").and_then(Value::as_str) == Some(command))
        .count()
}

fn hook_registration_matches(
    hooks: &Map<String, Value>,
    event: &str,
    group_index: usize,
    handler_index: usize,
    spec: &HookSpec,
) -> bool {
    let Some(group) = hooks
        .get(event)
        .and_then(Value::as_array)
        .and_then(|groups| groups.get(group_index))
    else {
        return false;
    };
    let matcher_matches = match spec.matcher {
        Some(matcher) => group.get("matcher").and_then(Value::as_str) == Some(matcher),
        None => group.get("matcher").is_none(),
    };
    let expected = json!({
        "type": "command",
        "command": spec.command,
        "timeout": HOOK_TIMEOUT_SECS,
    });
    matcher_matches
        && group
            .get("hooks")
            .and_then(Value::as_array)
            .and_then(|handlers| handlers.get(handler_index))
            == Some(&expected)
}

fn inspect_codex_config(
    path: &Path,
    hooks_path: &Path,
    specs: &[HookSpec],
    positions: &[Option<(usize, usize)>],
    warnings: &mut Vec<String>,
) -> (bool, bool) {
    let content = match read_status_file(path, "Codex config.toml", warnings) {
        ReadState::Missing => return (false, false),
        ReadState::Unreadable => return (true, false),
        ReadState::Content(content) => content,
    };
    let document: toml::Value = match content.parse() {
        Ok(document) => document,
        Err(_error) => {
            warnings.push("Codex config.toml is malformed".to_owned());
            return (true, false);
        }
    };
    let hooks_enabled = document
        .get("features")
        .and_then(toml::Value::as_table)
        .and_then(|features| features.get("hooks"))
        .and_then(toml::Value::as_bool)
        == Some(true);
    if !hooks_enabled {
        warnings.push("Codex hooks feature is not enabled".to_owned());
    }

    let trust_state = document
        .get("hooks")
        .and_then(toml::Value::as_table)
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table);
    let trust_prefix = format!("{}:", hooks_path.display());
    let mut footprint = trust_state.is_some_and(|state| {
        state.keys().any(|key| {
            key.starts_with(&trust_prefix)
                && specs
                    .iter()
                    .filter_map(|spec| spec.trust_event)
                    .any(|event| key[trust_prefix.len()..].starts_with(&format!("{event}:")))
        })
    });
    let mut trust_current = true;
    for (spec, position) in specs.iter().zip(positions) {
        let Some(trust_event) = spec.trust_event else {
            continue;
        };
        let valid = position.is_some_and(|(group_index, handler_index)| {
            let trust_key =
                codex_hook_trust_key(hooks_path, trust_event, group_index, handler_index);
            footprint |= trust_state.is_some_and(|state| state.contains_key(&trust_key));
            let expected = codex_command_hook_trusted_hash(
                trust_event,
                &spec.command,
                HOOK_TIMEOUT_SECS,
                None,
            )
            .ok();
            trust_state
                .and_then(|state| state.get(&trust_key))
                .and_then(toml::Value::as_table)
                .and_then(|entry| entry.get("trusted_hash"))
                .and_then(toml::Value::as_str)
                .zip(expected.as_deref())
                .is_some_and(|(actual, expected)| actual == expected)
        });
        if !valid {
            warnings.push(format!(
                "managed {} trust record is missing or modified",
                spec.label
            ));
        }
        trust_current &= valid;
    }
    (footprint, hooks_enabled && trust_current)
}

fn read_status_file(path: &Path, label: &str, warnings: &mut Vec<String>) -> ReadState {
    match fs::read_to_string(path) {
        Ok(content) => ReadState::Content(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warnings.push(format!("{label} is missing"));
            ReadState::Missing
        }
        Err(_error) => {
            warnings.push(format!("{label} could not be read"));
            ReadState::Unreadable
        }
    }
}

/// Install for every supported agent whose config dir exists.
fn install_all_present() -> Result<Vec<IntegrationInstallReport>, ProtocolError> {
    let mut installed = Vec::new();
    let claude_dir = claude_config_dir()?;
    if claude_dir.is_dir() {
        installed.push(report(AgentKind::Claude, &install_claude(&claude_dir)?));
    }
    let codex_dir = codex_config_dir()?;
    if codex_dir.is_dir() {
        installed.push(report(AgentKind::Codex, &install_codex(&codex_dir)?));
    }
    if installed.is_empty() {
        return Err(ProtocolError::new(
            ErrorClass::Runtime,
            "agent_config_dir_missing",
            format!(
                "no agent config dir found (looked for {} and {})",
                claude_dir.display(),
                codex_dir.display()
            ),
            Some("install Claude Code or Codex first".to_owned()),
        ));
    }
    Ok(installed)
}

fn report(agent: AgentKind, paths: &InstallPaths) -> IntegrationInstallReport {
    IntegrationInstallReport {
        agent,
        hook_path: paths.hook_path.display().to_string(),
        config_paths: paths
            .config_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    }
}

/// Resolve Claude's config dir: `$CLAUDE_CONFIG_DIR` or `~/.claude`.
pub fn claude_config_dir() -> Result<PathBuf, ProtocolError> {
    config_dir(CLAUDE_CONFIG_DIR_ENV, ".claude")
}

/// Resolve Codex's config dir: `$CODEX_HOME` or `~/.codex`.
pub fn codex_config_dir() -> Result<PathBuf, ProtocolError> {
    config_dir(CODEX_HOME_ENV, ".codex")
}

/// Install the Claude hooks into `claude_dir`.
///
/// Writes managed scripts under `hooks/` and merges `SessionStart`,
/// `SessionEnd`,
/// `Notification`, `Stop`, and `StopFailure` hooks into `settings.json`,
/// stripping any hooks this installer owns first so reinstall is idempotent.
///
/// # Errors
///
/// Fails fast if `claude_dir` is absent, if `settings.json` is malformed, or on
/// any I/O error.
pub fn install_claude(claude_dir: &Path) -> Result<InstallPaths, ProtocolError> {
    if !claude_dir.is_dir() {
        return Err(config_dir_missing(&AgentKind::Claude, claude_dir));
    }

    let hooks_dir = claude_dir.join("hooks");
    create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join(STATE_HOOK_INSTALL_NAME);
    write_file(&hook_path, CLAUDE_HOOK_ASSET)?;
    make_executable(&hook_path)?;
    let notify_hook_path = hooks_dir.join(NOTIFY_HOOK_INSTALL_NAME);
    write_file(&notify_hook_path, CLAUDE_NOTIFY_HOOK_ASSET)?;
    make_executable(&notify_hook_path)?;

    let settings_path = claude_dir.join("settings.json");
    let mut settings = read_json_object_or_empty(&settings_path)?;
    let hooks = ensure_hooks_object(&mut settings, &settings_path)?;
    let state_hook_commands = [
        hook_command(&hook_path, HOOK_ACTION),
        hook_command(&hook_path, HOOK_RELEASE_ACTION),
    ];
    remove_owned_command_hooks(hooks, &state_hook_commands);
    remove_owned_command_hooks(hooks, &claude_notify_hook_commands(&notify_hook_path));
    ensure_command_hook(
        hooks,
        SESSION_START_EVENT,
        &state_hook_commands[0],
        Some("*"),
    )?;
    ensure_command_hook(hooks, SESSION_END_EVENT, &state_hook_commands[1], Some("*"))?;
    for matcher in CLAUDE_NOTIFICATION_MATCHERS {
        ensure_command_hook(
            hooks,
            CLAUDE_NOTIFICATION_EVENT,
            &hook_command_with_args(&notify_hook_path, &[NOTIFICATION_ACTION, matcher]),
            Some(matcher),
        )?;
    }
    ensure_command_hook(
        hooks,
        CLAUDE_STOP_EVENT,
        &hook_command(&notify_hook_path, STOP_ACTION),
        Some("*"),
    )?;
    ensure_command_hook(
        hooks,
        CLAUDE_STOP_FAILURE_EVENT,
        &hook_command(&notify_hook_path, STOP_FAILURE_ACTION),
        Some("*"),
    )?;
    write_json_pretty(&settings_path, &settings)?;

    Ok(InstallPaths {
        hook_path,
        config_paths: vec![settings_path],
    })
}

/// Install the Codex hooks into `codex_dir`.
///
/// Writes managed scripts, merges `SessionStart`, `PermissionRequest`, and
/// `Stop` hooks into `hooks.json`, and enables `[features] hooks = true` in
/// `config.toml`, idempotently.
///
/// # Errors
///
/// Fails fast if `codex_dir` is absent, if `hooks.json` is malformed, or on any
/// I/O error.
pub fn install_codex(codex_dir: &Path) -> Result<InstallPaths, ProtocolError> {
    if !codex_dir.is_dir() {
        return Err(config_dir_missing(&AgentKind::Codex, codex_dir));
    }

    let hook_path = codex_dir.join(STATE_HOOK_INSTALL_NAME);
    write_file(&hook_path, CODEX_HOOK_ASSET)?;
    make_executable(&hook_path)?;
    let notify_hook_path = codex_dir.join(NOTIFY_HOOK_INSTALL_NAME);
    write_file(&notify_hook_path, CODEX_NOTIFY_HOOK_ASSET)?;
    make_executable(&notify_hook_path)?;

    let hooks_path = codex_dir.join("hooks.json");
    let mut hooks_file = read_json_object_or_empty(&hooks_path)?;
    let hooks = ensure_hooks_object(&mut hooks_file, &hooks_path)?;
    remove_owned_command_hooks(hooks, &[hook_command(&hook_path, HOOK_ACTION)]);
    remove_owned_command_hooks(hooks, &codex_notify_hook_commands(&notify_hook_path));
    // Codex exposes `Stop` for turn completion, not session/process exit. Do
    // not wire state release to it; procwatch is the lifecycle backstop.
    let codex_hooks = [
        CodexManagedHook {
            event: SESSION_START_EVENT,
            trust_event: CODEX_SESSION_START_TRUST_EVENT,
            command: hook_command(&hook_path, HOOK_ACTION),
        },
        CodexManagedHook {
            event: CODEX_PERMISSION_REQUEST_EVENT,
            trust_event: CODEX_PERMISSION_REQUEST_TRUST_EVENT,
            command: hook_command(&notify_hook_path, PERMISSION_REQUEST_ACTION),
        },
        CodexManagedHook {
            event: CODEX_STOP_EVENT,
            trust_event: CODEX_STOP_TRUST_EVENT,
            command: hook_command(&notify_hook_path, STOP_ACTION),
        },
    ];
    for managed in &codex_hooks {
        ensure_command_hook(hooks, managed.event, &managed.command, None)?;
    }
    let mut trust_entries = Vec::with_capacity(codex_hooks.len());
    for managed in &codex_hooks {
        let (group_index, handler_index) =
            command_hook_position(hooks, managed.event, &managed.command).ok_or_else(|| {
                settings_invalid(
                    &hooks_path,
                    &format!(
                        "installed Codex {} hook was not found after merge",
                        managed.event
                    ),
                )
            })?;
        trust_entries.push((
            managed.trust_event,
            managed.command.clone(),
            group_index,
            handler_index,
        ));
    }
    write_json_pretty(&hooks_path, &hooks_file)?;

    let config_path = codex_dir.join("config.toml");
    let existing = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(io_error("read", &config_path, &err)),
    };
    let mut trust_states = Vec::with_capacity(trust_entries.len());
    for (trust_event, command, group_index, handler_index) in trust_entries {
        let trust_key = codex_hook_trust_key(&hooks_path, trust_event, group_index, handler_index);
        let trusted_hash =
            codex_command_hook_trusted_hash(trust_event, &command, HOOK_TIMEOUT_SECS, None)?;
        trust_states.push((trust_key, trusted_hash));
    }
    let updated = update_codex_config_toml(&existing, &trust_states, &config_path)?;
    if updated != existing {
        write_file(&config_path, &updated)?;
    }

    Ok(InstallPaths {
        hook_path,
        config_paths: vec![hooks_path, config_path],
    })
}

struct CodexManagedHook {
    event: &'static str,
    trust_event: &'static str,
    command: String,
}

fn command_hook_position(
    hooks: &Map<String, Value>,
    event: &str,
    command: &str,
) -> Option<(usize, usize)> {
    hooks
        .get(event)?
        .as_array()?
        .iter()
        .enumerate()
        .find_map(|(group_index, group)| {
            group
                .get("hooks")?
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(handler_index, hook)| {
                    (hook.get("type").and_then(Value::as_str) == Some("command")
                        && hook.get("command").and_then(Value::as_str) == Some(command))
                    .then_some((group_index, handler_index))
                })
        })
}

fn codex_hook_trust_key(
    hooks_path: &Path,
    event_name: &str,
    group_index: usize,
    handler_index: usize,
) -> String {
    format!(
        "{}:{event_name}:{group_index}:{handler_index}",
        hooks_path.display()
    )
}

#[derive(Serialize)]
struct CodexNormalizedHookIdentity<'a> {
    event_name: &'a str,
    #[serde(flatten)]
    group: CodexMatcherGroup,
}

#[derive(Clone, Serialize)]
struct CodexMatcherGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<CodexHookHandlerConfig>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
enum CodexHookHandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default, rename = "commandWindows", alias = "command_windows")]
        command_windows: Option<String>,
        #[serde(default, rename = "timeout")]
        timeout_sec: Option<u64>,
        #[serde(default)]
        r#async: bool,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
    },
}

fn codex_command_hook_trusted_hash(
    event_name: &str,
    command: &str,
    timeout_sec: u64,
    matcher: Option<&str>,
) -> Result<String, ProtocolError> {
    let identity = CodexNormalizedHookIdentity {
        event_name,
        group: CodexMatcherGroup {
            matcher: matcher.map(ToOwned::to_owned),
            hooks: vec![CodexHookHandlerConfig::Command {
                command: command.to_owned(),
                command_windows: None,
                timeout_sec: Some(timeout_sec),
                r#async: false,
                status_message: None,
            }],
        },
    };
    let value = toml::Value::try_from(identity).map_err(|err| {
        ProtocolError::new(
            ErrorClass::Runtime,
            "integration_settings_invalid",
            format!("failed to serialize Codex hook trust identity: {err}"),
            None,
        )
    })?;
    Ok(version_for_toml(&value))
}

fn version_for_toml(value: &toml::Value) -> String {
    let json = serde_json::to_value(value).unwrap_or(Value::Null);
    let canonical = canonical_json(&json);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(hash.len() * 2);
    for byte in hash {
        write!(hex, "{byte:02x}").expect("writing to a String is infallible");
    }
    format!("sha256:{hex}")
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, canonical_json(value));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

/// Build the shell command string that runs our hook for one action.
fn hook_command(hook_path: &Path, action: &str) -> String {
    hook_command_with_args(hook_path, &[action])
}

/// Build the shell command string that runs our hook with fixed arguments.
fn hook_command_with_args(hook_path: &Path, args: &[&str]) -> String {
    let mut command = format!(
        "sh {}",
        shell_single_quote(&hook_path.display().to_string())
    );
    for arg in args {
        command.push(' ');
        command.push_str(arg);
    }
    command
}

fn claude_notify_hook_commands(notify_hook_path: &Path) -> Vec<String> {
    let mut commands = Vec::with_capacity(CLAUDE_NOTIFICATION_MATCHERS.len() + 2);
    for matcher in CLAUDE_NOTIFICATION_MATCHERS {
        commands.push(hook_command_with_args(
            notify_hook_path,
            &[NOTIFICATION_ACTION, matcher],
        ));
    }
    commands.push(hook_command(notify_hook_path, STOP_ACTION));
    commands.push(hook_command(notify_hook_path, STOP_FAILURE_ACTION));
    commands
}

fn codex_notify_hook_commands(notify_hook_path: &Path) -> Vec<String> {
    vec![
        hook_command(notify_hook_path, PERMISSION_REQUEST_ACTION),
        hook_command(notify_hook_path, STOP_ACTION),
    ]
}

/// Single-quote a value for a POSIX shell command line.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Get (or create) the `hooks` object inside an agent settings document.
fn ensure_hooks_object<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
) -> Result<&'a mut Map<String, Value>, ProtocolError> {
    let root = settings
        .as_object_mut()
        .ok_or_else(|| settings_invalid(settings_path, "top level must be a JSON object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    hooks
        .as_object_mut()
        .ok_or_else(|| settings_invalid(settings_path, "`hooks` must be a JSON object"))
}

/// Add a command hook in the nested agent format, deduped.
///
/// Nested shape (Claude/Codex):
/// `{ "matcher": "...", "hooks": [{ "type": "command", "command": "...", "timeout": N }] }`.
fn ensure_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    matcher: Option<&str>,
) -> Result<(), ProtocolError> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorClass::Runtime,
                "integration_settings_invalid",
                format!("hook entries for {event} must be an array"),
                None,
            )
        })?;

    let already_installed = entries.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hook_entries| {
                hook_entries.iter().any(|hook| {
                    hook.get("type").and_then(Value::as_str) == Some("command")
                        && hook.get("command").and_then(Value::as_str) == Some(command)
                })
            })
    });
    if already_installed {
        return Ok(());
    }

    let mut entry = Map::new();
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(
        "hooks".to_string(),
        json!([{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS }]),
    );
    entries.push(Value::Object(entry));
    Ok(())
}

/// Strip every exact command hook this installer owns.
///
/// Ownership is keyed on the precise command strings written by the installer,
/// so user hooks that merely mention the managed script path are preserved.
fn remove_owned_command_hooks(hooks: &mut Map<String, Value>, owned_commands: &[String]) {
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(entries) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain_mut(|entry| {
            let Some(entry_object) = entry.as_object_mut() else {
                return true;
            };
            let Some(hook_entries) = entry_object.get_mut("hooks").and_then(Value::as_array_mut)
            else {
                return true;
            };
            hook_entries.retain(|hook| !is_owned_command(hook, owned_commands));
            !hook_entries.is_empty()
        });
        if hooks
            .get(&event)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            hooks.remove(&event);
        }
    }
}

/// Whether a hook entry is one of this installer's exact command hooks.
fn is_owned_command(hook: &Value, owned_commands: &[String]) -> bool {
    hook.get("type").and_then(Value::as_str) == Some("command")
        && hook
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| owned_commands.iter().any(|owned| owned == command))
}

/// Update Codex `config.toml` through a TOML parser, preserving unrelated data.
///
/// Enables `[features] hooks = true` and records the trusted hash for every
/// managed hook under `[hooks.state.<trust_key>]` in a single parse pass, so a
/// multi-hook install (`SessionStart`, `PermissionRequest`, `Stop`, ...) trusts
/// each entry at once.
fn update_codex_config_toml(
    content: &str,
    trust_states: &[(String, String)],
    path: &Path,
) -> Result<String, ProtocolError> {
    let mut doc = content.parse::<DocumentMut>().map_err(|err| {
        settings_invalid(path, &format!("invalid TOML in Codex config.toml: {err}"))
    })?;

    ensure_table(doc.as_table_mut(), "features", path)?.insert("hooks", value(true));
    let hooks = ensure_table(doc.as_table_mut(), "hooks", path)?;
    let state = ensure_table(hooks, "state", path)?;
    for (trust_key, trusted_hash) in trust_states {
        ensure_table(state, trust_key, path)?.insert("trusted_hash", value(trusted_hash.as_str()));
    }

    Ok(doc.to_string())
}

fn ensure_table<'a>(
    parent: &'a mut Table,
    key: &str,
    path: &Path,
) -> Result<&'a mut Table, ProtocolError> {
    let item = parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    item.as_table_mut().ok_or_else(|| {
        settings_invalid(
            path,
            &format!("cannot update Codex config.toml: `{key}` is not a TOML table"),
        )
    })
}

#[cfg(test)]
fn toml_basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn config_dir(env_var: &str, home_relative: &str) -> Result<PathBuf, ProtocolError> {
    if let Some(value) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return Ok(expand_tilde(PathBuf::from(value)));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorClass::Configuration,
                "missing_env",
                format!("cannot resolve agent config dir: neither {env_var} nor HOME is set"),
                None,
            )
        })?;
    Ok(PathBuf::from(home).join(home_relative))
}

/// Expand a leading `~`/`~/` against `$HOME`.
fn expand_tilde(path: PathBuf) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
        return path;
    };
    if raw == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return PathBuf::from(home).join(rest);
    }
    path
}

fn read_json_object_or_empty(path: &Path) -> Result<Value, ProtocolError> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<Value>(&content)
            .map_err(|err| settings_invalid(path, &format!("invalid JSON: {err}"))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(io_error("read", path, &err)),
    }
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<(), ProtocolError> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|err| settings_invalid(path, &format!("could not serialize settings: {err}")))?;
    write_file(path, &body)
}

fn write_file(path: &Path, body: &str) -> Result<(), ProtocolError> {
    fs::write(path, body).map_err(|err| io_error("write", path, &err))
}

fn create_dir_all(path: &Path) -> Result<(), ProtocolError> {
    fs::create_dir_all(path).map_err(|err| io_error("create directory", path, &err))
}

fn make_executable(path: &Path) -> Result<(), ProtocolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = fs::metadata(path)
            .map_err(|err| io_error("stat", path, &err))?
            .permissions();
        perms.set_mode(MANAGED_HOOK_MODE);
        fs::set_permissions(path, perms).map_err(|err| io_error("chmod", path, &err))?;
    };
    let _ = path;
    Ok(())
}

fn config_dir_missing(agent: &AgentKind, dir: &Path) -> ProtocolError {
    let (name, hint) = match agent {
        AgentKind::Claude => ("claude", "install Claude Code first"),
        AgentKind::Codex => ("codex", "install Codex first"),
        AgentKind::Shell => ("shell", "shells have no hook integration"),
        AgentKind::Hermes => (
            "hermes",
            "Hermes integration is not available in this milestone",
        ),
        AgentKind::Unknown(ref value) => {
            return ProtocolError::agent_kind_unsupported(value);
        }
    };
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_config_dir_missing",
        format!("{name} config dir not found at {}", dir.display()),
        Some(hint.to_owned()),
    )
}

fn settings_invalid(path: &Path, message: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_settings_invalid",
        format!("{}: {message}", path.display()),
        None,
    )
}

fn io_error(action: &str, path: &Path, source: &io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_io_failed",
        format!("failed to {action} {}: {source}", path.display()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::method;
    use protocol::AgentKind;
    use serde_json::{json, Value};

    use super::{
        codex_command_hook_trusted_hash, codex_hook_trust_key, hook_command, install_claude,
        install_codex, shell_single_quote, toml_basic_string, CLAUDE_HOOK_ASSET,
        CLAUDE_NOTIFY_HOOK_ASSET, CODEX_HOOK_ASSET, CODEX_NOTIFY_HOOK_ASSET,
        CODEX_SESSION_START_TRUST_EVENT, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID,
        ENV_SOCKET_PATH, EXPECTED_INTEGRATION_VERSION, HOOK_TIMEOUT_SECS, SESSION_START_EVENT,
        STATE_HOOK_INSTALL_NAME,
    };

    /// Large enough to expose unbounded hook stdin reads through pipe backpressure.
    const LARGE_HOOK_INPUT_BYTES: usize = 1024 * 1024;
    /// Minimal successful JSON-RPC response expected by notification hook scripts.
    const HOOK_RESPONSE: &[u8] = b"{\"v\":1,\"id\":\"test\",\"result\":{}}\n";
    /// Successful worker-private identity response expected by state hooks.
    const WORKER_HOOK_SUCCESS_RESPONSE: &[u8] =
        b"{\"ok\":true,\"launch_identity_accepted\":true}\n";
    /// Rejected worker-private identity response that must trigger public fallback.
    const WORKER_HOOK_FAILURE_RESPONSE: &[u8] =
        b"{\"ok\":false,\"launch_identity_accepted\":false}\n";
    /// Maximum time a hook test waits for expected Unix-socket callbacks.
    const HOOK_CAPTURE_TIMEOUT_SECS: u64 = 2;
    /// Poll interval for nonblocking hook socket accept loops.
    const HOOK_CAPTURE_POLL_MS: u64 = 10;
    /// State-hook requests expected from a successful `SessionStart` callback.
    const STATE_SESSION_REQUEST_COUNT: usize = 2;
    /// State-hook requests expected from a successful release callback.
    const STATE_RELEASE_REQUEST_COUNT: usize = 1;
    /// Integration asset version expected after PID-bearing state hooks ship.
    const STATE_ASSET_VERSION_HEADER: &str = "# POHUNEK_INTEGRATION_VERSION=4";
    /// Action argument for state-hook `SessionStart` reporting.
    const STATE_SESSION_ACTION: &str = "session";
    /// Action argument for state-hook release reporting.
    const STATE_RELEASE_ACTION: &str = "release";
    /// Claude lifecycle event used for active-agent release.
    const CLAUDE_SESSION_END_EVENT: &str = "SessionEnd";

    /// Per-process sequence that keeps parallel temp paths collision-free.
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pohunek-integration-{tag}-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read json")).expect("parse json")
    }

    fn session_start_command_hooks(settings: &Value) -> Vec<String> {
        command_hooks(settings, "SessionStart")
    }

    fn command_hooks(settings: &Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn matcher_commands(settings: &Value, event: &str, matcher: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry.get("matcher").and_then(Value::as_str) == Some(matcher))
                    .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn daemon_manifest_asset(agent: &str, script: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/integration/assets")
            .join(agent)
            .join(script)
    }

    fn notification_asset(agent: &str) -> PathBuf {
        daemon_manifest_asset(agent, "pohunek-agent-notify.sh")
    }

    fn state_asset(agent: &str) -> PathBuf {
        daemon_manifest_asset(agent, "pohunek-agent-state.sh")
    }

    fn run_state_asset(
        agent: &str,
        args: &[&str],
        input: &Value,
        socket_available: bool,
        expected_requests: usize,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>) {
        let asset_path = state_asset(agent);
        assert!(
            asset_path.is_file(),
            "missing state hook asset at {}",
            asset_path.display()
        );

        let temp = temp_dir(&format!("{agent}-state-run"));
        let socket_path = temp.join("daemon.sock");
        let handle = socket_available.then(|| {
            let listener = UnixListener::bind(&socket_path).expect("bind hook socket");
            listener
                .set_nonblocking(true)
                .expect("make hook socket nonblocking");
            thread::spawn(move || {
                let deadline =
                    std::time::Instant::now() + Duration::from_secs(HOOK_CAPTURE_TIMEOUT_SECS);
                let mut requests = Vec::new();
                while requests.len() < expected_requests && std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _addr)) => {
                            let mut raw = Vec::new();
                            let mut byte = [0_u8; 1];
                            while stream.read(&mut byte).expect("read hook request") == 1 {
                                raw.push(byte[0]);
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            let request = serde_json::from_slice::<Value>(&raw)
                                .expect("hook request is JSON");
                            requests.push(request);
                            write_hook_response(&mut stream);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(HOOK_CAPTURE_POLL_MS));
                        }
                        Err(err) => panic!("accept hook request: {err}"),
                    }
                }
                requests
            })
        });

        let mut command = Command::new("sh");
        command
            .arg(&asset_path)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TMPDIR", &temp)
            .env(ENV_FLAG, "1")
            .env(ENV_SOCKET_PATH, &socket_path)
            .env(ENV_SESSION_ID, "session-123")
            .env(
                ENV_PROTOCOL_VERSION,
                protocol::PROTOCOL_VERSION.get().to_string(),
            )
            .env("POHUNEK_RUNTIME_ID", "runtime-123")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn state hook");
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input.to_string().as_bytes())
                .expect("write state hook stdin");
        }
        let output = child.wait_with_output().expect("wait for state hook");

        let requests = handle
            .map(|handle| handle.join().expect("hook socket thread"))
            .unwrap_or_default();
        (
            output.status,
            String::from_utf8(output.stdout).expect("hook stdout utf8"),
            String::from_utf8(output.stderr).expect("hook stderr utf8"),
            requests,
        )
    }

    fn run_worker_state_asset(agent: &str, action: &str, input: &Value) -> Value {
        let (request, public_requests) = run_worker_state_asset_with_response(
            agent,
            action,
            input,
            WORKER_HOOK_SUCCESS_RESPONSE,
            0,
        );
        assert!(
            public_requests.is_empty(),
            "accepted worker request must not use public fallback"
        );
        request
    }

    fn run_worker_state_asset_with_response(
        agent: &str,
        action: &str,
        input: &Value,
        worker_response: &'static [u8],
        expected_public_requests: usize,
    ) -> (Value, Vec<Value>) {
        let asset_path = state_asset(agent);
        let temp = temp_dir(&format!("{agent}-worker-state-run"));
        let worker_socket = temp.join("worker.sock");
        let daemon_socket = temp.join("daemon.sock");
        let listener = UnixListener::bind(&worker_socket).expect("bind worker hook socket");
        let capture = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept worker hook");
            let mut raw = Vec::new();
            let mut byte = [0_u8; 1];
            while stream.read(&mut byte).expect("read worker hook") == 1 {
                raw.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            stream
                .write_all(worker_response)
                .expect("write worker hook response");
            serde_json::from_slice::<Value>(&raw).expect("worker hook JSON")
        });
        let daemon_listener = UnixListener::bind(&daemon_socket).expect("bind daemon hook socket");
        daemon_listener
            .set_nonblocking(true)
            .expect("make daemon hook socket nonblocking");
        let daemon_capture = thread::spawn(move || {
            let deadline =
                std::time::Instant::now() + Duration::from_secs(HOOK_CAPTURE_TIMEOUT_SECS);
            let mut requests = Vec::new();
            while requests.len() < expected_public_requests && std::time::Instant::now() < deadline
            {
                match daemon_listener.accept() {
                    Ok((mut stream, _addr)) => {
                        let mut raw = Vec::new();
                        let mut byte = [0_u8; 1];
                        while stream.read(&mut byte).expect("read daemon hook") == 1 {
                            raw.push(byte[0]);
                            if byte[0] == b'\n' {
                                break;
                            }
                        }
                        requests
                            .push(serde_json::from_slice::<Value>(&raw).expect("daemon hook JSON"));
                        write_hook_response(&mut stream);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(HOOK_CAPTURE_POLL_MS));
                    }
                    Err(err) => panic!("accept daemon hook request: {err}"),
                }
            }
            requests
        });

        let mut child = Command::new("sh")
            .arg(asset_path)
            .arg(action)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TMPDIR", &temp)
            .env(ENV_FLAG, "1")
            .env("POHUNEK_WORKER_SOCKET_PATH", &worker_socket)
            .env("POHUNEK_NATIVE_REFERENCE_KIND", "id")
            .env(ENV_SOCKET_PATH, &daemon_socket)
            .env(ENV_SESSION_ID, "session-123")
            .env(
                ENV_PROTOCOL_VERSION,
                protocol::PROTOCOL_VERSION.get().to_string(),
            )
            .env("POHUNEK_RUNTIME_ID", "runtime-123")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worker state hook");
        child
            .stdin
            .take()
            .expect("hook stdin")
            .write_all(input.to_string().as_bytes())
            .expect("write hook stdin");
        let output = child.wait_with_output().expect("wait for worker hook");
        assert!(
            output.status.success(),
            "{agent} worker hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "worker hook must be silent");
        (
            capture.join().expect("worker hook capture"),
            daemon_capture.join().expect("daemon hook capture"),
        )
    }

    fn run_notification_asset(
        agent: &str,
        args: &[&str],
        input: &Value,
        socket_available: bool,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>) {
        let (status, stdout, stderr, requests, _bytes_written) = run_notification_asset_custom(
            agent,
            args,
            input.to_string().as_bytes(),
            socket_available,
            Some("session-123"),
            None,
            false,
        );
        (status, stdout, stderr, requests)
    }

    fn run_notification_asset_custom(
        agent: &str,
        args: &[&str],
        input: &[u8],
        socket_available: bool,
        session_id: Option<&str>,
        tmpdir_override: Option<&Path>,
        allow_broken_pipe: bool,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>, usize) {
        let asset_path = notification_asset(agent);
        assert!(
            asset_path.is_file(),
            "missing notification hook asset at {}",
            asset_path.display()
        );

        let temp = temp_dir(&format!("{agent}-notify-run"));
        let socket_path = temp.join("daemon.sock");
        let tmpdir = tmpdir_override.unwrap_or(&temp);
        let handle = socket_available.then(|| {
            let listener = UnixListener::bind(&socket_path).expect("bind hook socket");
            listener
                .set_nonblocking(true)
                .expect("make hook socket nonblocking");
            thread::spawn(move || {
                let deadline =
                    std::time::Instant::now() + Duration::from_secs(HOOK_CAPTURE_TIMEOUT_SECS);
                let mut requests = Vec::new();
                while std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _addr)) => {
                            let mut raw = Vec::new();
                            let mut byte = [0_u8; 1];
                            while stream.read(&mut byte).expect("read hook request") == 1 {
                                raw.push(byte[0]);
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            let request = serde_json::from_slice::<Value>(&raw)
                                .expect("hook request is JSON");
                            requests.push(request);
                            write_hook_response(&mut stream);
                            break;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(HOOK_CAPTURE_POLL_MS));
                        }
                        Err(err) => panic!("accept hook request: {err}"),
                    }
                }
                requests
            })
        });

        let mut command = Command::new("sh");
        command
            .arg(&asset_path)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TMPDIR", tmpdir)
            .env(ENV_FLAG, "1")
            .env(ENV_SOCKET_PATH, &socket_path)
            .env(ENV_PROTOCOL_VERSION, "1")
            .env("POHUNEK_SECRET_SENTINEL", "DROP_ME_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session_id) = session_id {
            command.env(ENV_SESSION_ID, session_id);
        }
        let mut child = command.spawn().expect("spawn notification hook");
        let mut bytes_written = 0;
        if let Some(mut stdin) = child.stdin.take() {
            while bytes_written < input.len() {
                match stdin.write(&input[bytes_written..]) {
                    Ok(0) => break,
                    Ok(written) => bytes_written += written,
                    Err(err) if allow_broken_pipe && err.kind() == ErrorKind::BrokenPipe => break,
                    Err(err) => panic!("write hook stdin: {err}"),
                }
            }
        }
        let output = child
            .wait_with_output()
            .expect("wait for notification hook");
        let requests = handle
            .map(|handle| handle.join().expect("hook socket thread"))
            .unwrap_or_default();
        (
            output.status,
            String::from_utf8(output.stdout).expect("hook stdout utf8"),
            String::from_utf8(output.stderr).expect("hook stderr utf8"),
            requests,
            bytes_written,
        )
    }

    fn captured_notification_request(
        agent: &str,
        args: &[&str],
        input: &Value,
    ) -> (String, String, Value) {
        let (status, stdout, stderr, requests) = run_notification_asset(agent, args, input, true);
        assert!(status.success(), "hook exited with {status}: {stderr}");
        assert_eq!(stdout, "", "hook must not print stdout");
        assert_eq!(stderr, "", "hook must not print stderr");
        assert_eq!(requests.len(), 1, "expected one request: {requests:?}");
        (stdout, stderr, requests.into_iter().next().unwrap())
    }

    fn large_json_input() -> Vec<u8> {
        let mut input = br#"{"hook_event_id":"large"}"#.to_vec();
        input.resize(LARGE_HOOK_INPUT_BYTES, b' ');
        input
    }

    fn write_hook_response(stream: &mut impl Write) {
        if let Err(err) = stream.write_all(HOOK_RESPONSE) {
            // Notification hooks are fire-and-forget; after the request line is
            // captured, the hook may close without reading the daemon response.
            assert!(
                matches!(
                    err.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                ),
                "write hook response: {err}"
            );
        }
    }

    struct DisconnectingWriter {
        kind: ErrorKind,
    }

    impl Write for DisconnectingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(self.kind))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn hook_response_write_includes_newline() {
        let mut response = Vec::new();

        write_hook_response(&mut response);

        assert_eq!(response.as_slice(), HOOK_RESPONSE);
    }

    #[test]
    fn hook_response_write_tolerates_client_disconnect() {
        for kind in [ErrorKind::BrokenPipe, ErrorKind::ConnectionReset] {
            let mut writer = DisconnectingWriter { kind };

            write_hook_response(&mut writer);
        }
    }

    fn assert_notification_payload(
        request: &Value,
        agent: &str,
        provider_event: &str,
        kind: &str,
        severity: &str,
        matcher: Option<&str>,
        expected_dedupe_key: Option<&str>,
    ) {
        assert_eq!(request["method"], json!(method::NOTIFICATION_CREATE));
        let params = &request["params"];
        assert_eq!(params["kind"], json!(kind));
        assert_eq!(params["severity"], json!(severity));
        assert_eq!(params["status"], json!("unread"));
        assert_eq!(params["session_id"], json!("session-123"));
        assert_eq!(params["agent_kind"], json!(agent));
        assert_eq!(params["source"]["provider"], json!(agent));
        assert_eq!(params["source"]["provider_event"], json!(provider_event));
        assert_eq!(params["metadata"]["provider"], json!(agent));
        assert_eq!(params["metadata"]["provider_event"], json!(provider_event));
        if let Some(matcher) = matcher {
            assert_eq!(params["metadata"]["matcher"], json!(matcher));
        } else {
            assert!(params["metadata"].get("matcher").is_none());
        }
        match expected_dedupe_key {
            Some(expected) => assert_eq!(params["dedupe_key"], json!(expected)),
            None => assert!(params.get("dedupe_key").is_none()),
        }

        let source_id = params["source_id"].as_str().expect("source_id is a string");
        assert!(
            source_id.starts_with(&format!("hook:{agent}:{provider_event}:")),
            "unexpected source_id: {source_id}"
        );
        assert_eq!(params["source"]["host_local_source_id"], json!(source_id));
    }

    #[test]
    fn notification_hooks_drop_hostile_session_id_before_wire() {
        for (agent, args) in [
            ("codex", vec!["permission_request"]),
            ("claude", vec!["notification", "permission_prompt"]),
        ] {
            let (status, stdout, stderr, requests, _bytes_written) = run_notification_asset_custom(
                agent,
                &args,
                br#"{"hook_event_id":"hostile-session"}"#,
                true,
                Some("session-123\nhostile"),
                None,
                false,
            );

            assert!(
                status.success(),
                "{agent} hook exited with {status}: {stderr}"
            );
            assert_eq!(stdout, "", "{agent} hook must not print stdout");
            assert_eq!(stderr, "", "{agent} hook must not print stderr");
            assert_eq!(
                requests.len(),
                1,
                "{agent} hook must still send notification"
            );
            let params = &requests[0]["params"];
            assert!(
                params.get("session_id").is_none(),
                "{agent} hook must drop hostile session_id: {params}"
            );
            assert!(
                params.get("dedupe_key").is_none(),
                "{agent} hook must not derive dedupe_key from hostile session_id: {params}"
            );
        }
    }

    #[test]
    fn notification_hooks_ignore_unknown_action_without_consuming_large_stdin() {
        let input = vec![b'x'; LARGE_HOOK_INPUT_BYTES];
        for (agent, args) in [
            ("codex", vec!["ignored_action"]),
            ("claude", vec!["ignored_action"]),
        ] {
            let (status, stdout, stderr, requests, bytes_written) = run_notification_asset_custom(
                agent,
                &args,
                &input,
                false,
                Some("session-123"),
                None,
                true,
            );

            assert!(
                status.success(),
                "{agent} hook exited with {status}: {stderr}"
            );
            assert_eq!(stdout, "", "{agent} hook must not print stdout");
            assert_eq!(stderr, "", "{agent} hook must not print stderr");
            assert!(requests.is_empty(), "{agent} ignored action must not send");
            assert!(
                bytes_written < LARGE_HOOK_INPUT_BYTES,
                "{agent} ignored action consumed the full oversized stdin"
            );
        }
    }

    #[test]
    fn notification_hooks_cap_large_valid_stdin() {
        let input = large_json_input();
        for (agent, args) in [
            ("codex", vec!["permission_request"]),
            ("claude", vec!["notification", "permission_prompt"]),
        ] {
            let (status, stdout, stderr, requests, bytes_written) = run_notification_asset_custom(
                agent,
                &args,
                &input,
                true,
                Some("session-123"),
                None,
                true,
            );

            assert!(
                status.success(),
                "{agent} hook exited with {status}: {stderr}"
            );
            assert_eq!(stdout, "", "{agent} hook must not print stdout");
            assert_eq!(stderr, "", "{agent} hook must not print stderr");
            assert_eq!(requests.len(), 1, "{agent} hook must send one request");
            assert!(
                bytes_written < LARGE_HOOK_INPUT_BYTES,
                "{agent} hook consumed the full oversized stdin"
            );
        }
    }

    #[test]
    fn notification_hooks_silence_mktemp_failure() {
        for (agent, args) in [
            ("codex", vec!["permission_request"]),
            ("claude", vec!["notification", "permission_prompt"]),
        ] {
            let temp = temp_dir(&format!("{agent}-broken-tmpdir"));
            let missing_tmpdir = temp.join("missing");
            let (status, stdout, stderr, requests, _bytes_written) = run_notification_asset_custom(
                agent,
                &args,
                br#"{"hook_event_id":"broken-tmpdir"}"#,
                false,
                Some("session-123"),
                Some(&missing_tmpdir),
                true,
            );

            assert!(
                status.success(),
                "{agent} hook exited with {status}: {stderr}"
            );
            assert_eq!(stdout, "", "{agent} hook must not print stdout");
            assert_eq!(stderr, "", "{agent} hook must not print stderr");
            assert!(requests.is_empty(), "{agent} hook must fail closed");
        }
    }

    #[test]
    fn codex_hook_trust_hash_matches_codex_normalized_identity() {
        let hash = codex_command_hook_trusted_hash(
            CODEX_SESSION_START_TRUST_EVENT,
            "sh '/tmp/pohunek-agent-state.sh' session",
            10,
            None,
        )
        .expect("hash Codex hook identity");

        assert_eq!(
            hash,
            "sha256:93067e645008b68a24d9341f188d245c8491bf9667f89b470391737e93dbe0d4"
        );
    }

    #[test]
    fn assets_fire_active_agent_then_native_id_with_our_env_and_exit_zero_on_missing_env() {
        for (agent, asset) in [("claude", CLAUDE_HOOK_ASSET), ("codex", CODEX_HOOK_ASSET)] {
            assert!(
                asset.starts_with("#!/bin/sh"),
                "hook must be a POSIX sh script"
            );
            assert!(
                asset.contains(STATE_ASSET_VERSION_HEADER),
                "{agent} hook must carry integration version 3"
            );
            assert!(
                asset.contains(method::SESSION_REPORT_AGENT),
                "hook must fire our active-agent method"
            );
            assert!(
                asset.contains(method::SESSION_REPORT_NATIVE_ID),
                "hook must fire our native-id method"
            );
            let report_agent_index = asset
                .find(method::SESSION_REPORT_AGENT)
                .expect("asset contains active-agent method");
            let report_native_index = asset
                .find(method::SESSION_REPORT_NATIVE_ID)
                .expect("asset contains native-id method");
            assert!(
                report_agent_index < report_native_index,
                "hook must report active agent before native id"
            );
            assert!(
                asset.contains("native_id_params[\"transcript_path\"] = transcript_path"),
                "hook must forward transcript_path to native-id reports for path-kind resume"
            );
            assert!(
                asset.contains("POHUNEK_AGENT_PID"),
                "{agent} hook must pass the parent agent pid into Python"
            );
            assert!(
                asset.contains("report_agent_params[\"pid\"] = agent_pid"),
                "{agent} hook must include a parsed pid in active-agent reports"
            );
            for env_name in [
                ENV_FLAG,
                ENV_SOCKET_PATH,
                ENV_SESSION_ID,
                ENV_PROTOCOL_VERSION,
            ] {
                assert!(
                    asset.contains(env_name),
                    "hook must reference handshake env {env_name}"
                );
            }
            // Missing handshake env / runtime must be a silent no-op.
            assert!(
                asset.contains("|| exit 0"),
                "hook must no-op (exit 0) when prerequisites are missing"
            );
            assert!(
                asset.contains("command -v python3"),
                "hook must guard on python3 availability"
            );
            // The terminal python invocation itself must be exit-0-guarded so an
            // abnormal interpreter exit (OOM, hook timeout kill) under `set -e`
            // never propagates a non-zero status that could break the agent.
            assert!(
                asset.contains("python3 - <<'PY' || exit 0"),
                "the python heredoc must be guarded with `|| exit 0`"
            );
        }

        assert!(
            CLAUDE_HOOK_ASSET.contains(method::SESSION_RELEASE_AGENT),
            "Claude state hook must expose a release path"
        );
        assert!(
            CLAUDE_HOOK_ASSET.contains(STATE_RELEASE_ACTION),
            "Claude state hook must accept the release action"
        );
    }

    #[test]
    fn state_hooks_send_pid_on_session_start_reports() {
        for agent in ["claude", "codex"] {
            let input = json!({
                "session_id": format!("{agent}-native"),
                "transcript_path": format!("/tmp/{agent}-transcript.jsonl"),
            });

            let (status, stdout, stderr, requests) = run_state_asset(
                agent,
                &[STATE_SESSION_ACTION],
                &input,
                true,
                STATE_SESSION_REQUEST_COUNT,
            );

            assert!(
                status.success(),
                "{agent} state hook exited with {status}: {stderr}"
            );
            assert_eq!(stdout, "", "{agent} state hook must not print stdout");
            assert_eq!(stderr, "", "{agent} state hook must not print stderr");
            assert_eq!(
                requests.len(),
                STATE_SESSION_REQUEST_COUNT,
                "{agent} state hook must send active-agent and native-id requests"
            );

            let report = &requests[0];
            assert_eq!(report["method"], json!(method::SESSION_REPORT_AGENT));
            assert_eq!(report["params"]["session_id"], json!("session-123"));
            assert_eq!(
                report["params"]["source"],
                json!(format!("pohunek:{agent}"))
            );
            assert_eq!(report["params"]["agent"], json!(agent));
            assert_eq!(report["params"]["pid"], json!(std::process::id()));
            assert_eq!(
                report["params"]["agent_session_id"],
                json!(format!("{agent}-native"))
            );
            assert_eq!(
                report["params"]["agent_session_path"],
                json!(format!("/tmp/{agent}-transcript.jsonl"))
            );

            let native = &requests[1];
            assert_eq!(native["method"], json!(method::SESSION_REPORT_NATIVE_ID));
            assert_eq!(
                native["v"],
                json!({
                    "minimum": protocol::PROTOCOL_VERSION.get(),
                    "maximum": protocol::PROTOCOL_VERSION.get(),
                })
            );
            assert_eq!(native["params"]["session_id"], json!("session-123"));
            assert_eq!(native["params"]["runtime_id"], json!("runtime-123"));
            assert_eq!(native["params"]["agent"], json!(agent));
            assert_eq!(native["params"]["pid"], json!(std::process::id()));
            assert!(native["params"]["pid_start_identity"].as_str().is_some());
            assert!(native["params"]["sequence"].as_str().is_some());
            assert!(native["params"]["expires_at"].as_str().is_some());
            assert_eq!(
                native["params"]["native_session_id"],
                json!(format!("{agent}-native"))
            );
            assert_eq!(
                native["params"]["transcript_path"],
                json!(format!("/tmp/{agent}-transcript.jsonl"))
            );
        }
    }

    #[test]
    fn state_hooks_prefer_worker_identity_protocol_and_release_active_claims() {
        for agent in ["claude", "codex"] {
            let native = format!("{agent}-native");
            let report = run_worker_state_asset(
                agent,
                STATE_SESSION_ACTION,
                &json!({
                    "session_id": native,
                    "transcript_path": format!("/tmp/{agent}.jsonl"),
                }),
            );
            assert_eq!(report["type"], "identity_report");
            assert_eq!(report["runtime_id"], "runtime-123");
            assert_eq!(report["provider"], agent);
            assert_eq!(report["reference_kind"], "id");
            assert_eq!(report["native_reference"], native);
            assert!(report["pid"].as_u64().is_some());
            assert!(report["start_identity"].as_u64().is_some());
            assert!(report["sequence"].as_u64().is_some());
            assert!(report["expires_at"].as_str().is_some());
        }

        let release = run_worker_state_asset(
            "claude",
            STATE_RELEASE_ACTION,
            &json!({"session_id": "claude-native"}),
        );
        assert_eq!(release["type"], "identity_release");
        assert_eq!(release["runtime_id"], "runtime-123");
        assert_eq!(release["provider"], "claude");
        assert!(release.get("native_reference").is_none());
    }

    #[test]
    fn rejected_worker_identity_reports_fall_back_to_public_methods() {
        for agent in ["claude", "codex"] {
            let (worker_request, public_requests) = run_worker_state_asset_with_response(
                agent,
                STATE_SESSION_ACTION,
                &json!({"session_id": format!("{agent}-native")}),
                WORKER_HOOK_FAILURE_RESPONSE,
                STATE_SESSION_REQUEST_COUNT,
            );

            assert_eq!(worker_request["type"], "identity_report");
            assert_eq!(public_requests.len(), STATE_SESSION_REQUEST_COUNT);
            assert_eq!(
                public_requests[0]["method"],
                json!(method::SESSION_REPORT_AGENT)
            );
            assert_eq!(
                public_requests[1]["method"],
                json!(method::SESSION_REPORT_NATIVE_ID)
            );
        }

        let (worker_request, public_requests) = run_worker_state_asset_with_response(
            "claude",
            STATE_RELEASE_ACTION,
            &json!({}),
            WORKER_HOOK_FAILURE_RESPONSE,
            STATE_RELEASE_REQUEST_COUNT,
        );
        assert_eq!(worker_request["type"], "identity_release");
        assert_eq!(public_requests.len(), STATE_RELEASE_REQUEST_COUNT);
        assert_eq!(
            public_requests[0]["method"],
            json!(method::SESSION_RELEASE_AGENT)
        );
    }

    #[test]
    fn claude_state_hook_release_sends_release_agent() {
        let (status, stdout, stderr, requests) = run_state_asset(
            "claude",
            &[STATE_RELEASE_ACTION],
            &json!({}),
            true,
            STATE_RELEASE_REQUEST_COUNT,
        );

        assert!(
            status.success(),
            "Claude hook exited with {status}: {stderr}"
        );
        assert_eq!(stdout, "", "Claude hook must not print stdout");
        assert_eq!(stderr, "", "Claude hook must not print stderr");
        assert_eq!(
            requests.len(),
            STATE_RELEASE_REQUEST_COUNT,
            "Claude release hook must send one release request"
        );

        let request = &requests[0];
        assert_eq!(request["method"], json!(method::SESSION_RELEASE_AGENT));
        assert_eq!(request["params"]["session_id"], json!("session-123"));
        assert_eq!(request["params"]["source"], json!("pohunek:claude"));
        assert_eq!(request["params"]["agent"], json!("claude"));
        assert!(
            request["params"]["seq"].as_u64().is_some(),
            "Claude release request must carry a fresh sequence"
        );
    }

    #[test]
    fn install_claude_into_fresh_dir_writes_executable_hook_and_session_start() {
        let claude_dir = temp_dir("claude-fresh");
        let paths = install_claude(&claude_dir).expect("install claude");

        assert!(paths.hook_path.is_file(), "hook script must be written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.hook_path)
                .expect("hook metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "hook must be executable");
        };

        let settings = read_json(&claude_dir.join("settings.json"));
        let commands = session_start_command_hooks(&settings);
        assert_eq!(commands.len(), 1, "exactly one SessionStart hook");
        assert!(commands[0].contains(paths.hook_path.to_str().unwrap()));
        // matcher is "*"
        assert_eq!(settings["hooks"]["SessionStart"][0]["matcher"], json!("*"));

        let release_commands = command_hooks(&settings, CLAUDE_SESSION_END_EVENT);
        assert_eq!(release_commands.len(), 1, "exactly one SessionEnd hook");
        assert!(release_commands[0].contains(paths.hook_path.to_str().unwrap()));
        assert!(
            release_commands[0].ends_with(STATE_RELEASE_ACTION),
            "SessionEnd hook must call release action: {release_commands:?}"
        );
        assert_eq!(
            settings["hooks"][CLAUDE_SESSION_END_EVENT][0]["matcher"],
            json!("*")
        );
    }

    #[test]
    fn status_reports_missing_config_without_mutation() {
        let root = temp_dir("status-missing");
        fs::create_dir_all(&root).expect("create temp root");
        let claude = root.join(".claude");
        let codex = root.join(".codex");

        let result = with_config_dirs(&claude, &codex, || {
            super::status(protocol::IntegrationStatusParams { agent: None })
        })
        .expect("status missing");

        assert_eq!(result.agents.len(), 2);
        for report in &result.agents {
            assert!(!report.available);
            assert_eq!(report.present_asset_paths, Vec::<String>::new());
            assert_eq!(report.expected_asset_paths.len(), 2);
            assert_eq!(report.installed_version, None);
            assert_eq!(report.expected_version, EXPECTED_INTEGRATION_VERSION);
            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::NotInstalled
            );
            assert_eq!(report.warnings.len(), 1);
        }
        assert!(!claude.exists());
        assert!(!codex.exists());
    }

    #[test]
    fn every_managed_asset_marker_matches_the_expected_version() {
        for (name, asset) in [
            ("Claude state", CLAUDE_HOOK_ASSET),
            ("Claude notification", CLAUDE_NOTIFY_HOOK_ASSET),
            ("Codex state", CODEX_HOOK_ASSET),
            ("Codex notification", CODEX_NOTIFY_HOOK_ASSET),
        ] {
            assert_eq!(
                super::parse_integration_version(asset),
                Some(EXPECTED_INTEGRATION_VERSION),
                "{name} marker must match the public expected version"
            );
        }
    }

    #[test]
    fn status_reports_complete_installs_current_without_mutation() {
        let root = temp_dir("status-current");
        let claude = root.join("claude");
        let codex = root.join("codex");
        fs::create_dir_all(&claude).expect("create Claude dir");
        fs::create_dir_all(&codex).expect("create Codex dir");
        install_claude(&claude).expect("install Claude fixture");
        install_codex(&codex).expect("install Codex fixture");
        let before = tree_snapshot(&root);

        let result = with_config_dirs(&claude, &codex, || {
            super::status(protocol::IntegrationStatusParams { agent: None })
        })
        .expect("status current");

        assert_eq!(tree_snapshot(&root), before, "status must not mutate files");
        assert_eq!(result.agents.len(), 2);
        for report in result.agents {
            assert_eq!(report.state, protocol::IntegrationInstallState::Current);
            assert_eq!(report.installed_version, Some(EXPECTED_INTEGRATION_VERSION));
            assert_eq!(report.expected_asset_paths.len(), 2);
            assert_eq!(report.present_asset_paths.len(), 2);
            assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        }
    }

    #[test]
    fn status_detects_each_modified_or_missing_managed_asset() {
        for (name, relative_path, mutation) in [
            (
                "state modified",
                "pohunek-agent-state.sh",
                Some("# modified\n"),
            ),
            (
                "notification modified",
                "pohunek-agent-notify.sh",
                Some("# modified\n"),
            ),
            ("state missing", "pohunek-agent-state.sh", None),
            ("notification missing", "pohunek-agent-notify.sh", None),
        ] {
            let codex = temp_dir(&format!("status-asset-{name}"));
            install_codex(&codex).expect("install Codex fixture");
            let path = codex.join(relative_path);
            match mutation {
                Some(content) => fs::write(&path, content).expect("modify managed asset"),
                None => fs::remove_file(&path).expect("remove managed asset"),
            }

            let report = explicit_status(&codex, AgentKind::Codex);

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{name}"
            );
            assert!(
                report.warnings.iter().any(|warning| warning.contains(
                    if relative_path.contains("notify") {
                        "notification hook"
                    } else {
                        "state hook"
                    }
                )),
                "{name}: {:?}",
                report.warnings
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn status_rejects_unsafe_managed_hook_permissions_with_repair_hint() {
        use std::os::unix::fs::PermissionsExt;

        let codex = temp_dir("status-unsafe-hook-permissions");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let mut permissions = fs::metadata(&hook_path)
            .expect("managed hook metadata")
            .permissions();
        permissions.set_mode(0o777);
        fs::set_permissions(&hook_path, permissions).expect("make managed hook unsafe");

        let report = explicit_status(&codex, AgentKind::Codex);

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        let warning = report
            .warnings
            .iter()
            .find(|warning| warning.contains("permissions are unsafe"))
            .expect("unsafe permissions warning");
        assert!(warning.contains("mode 0777"), "{warning}");
        assert!(
            warning.contains("pohunek integration install --agent codex"),
            "{warning}"
        );
    }

    #[test]
    fn status_detects_broken_claude_registration() {
        let claude = temp_dir("status-claude-registration");
        install_claude(&claude).expect("install Claude fixture");
        let settings_path = claude.join("settings.json");
        let mut settings = read_json(&settings_path);
        settings["hooks"][SESSION_START_EVENT][0]["hooks"][0]["timeout"] = json!(1);
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).expect("serialize modified settings"),
        )
        .expect("write modified settings");

        let report = explicit_status(&claude, AgentKind::Claude);

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("Claude SessionStart registration")));
    }

    #[test]
    fn status_rejects_managed_registration_under_an_extra_event() {
        let codex = temp_dir("status-extra-registration-event");
        install_codex(&codex).expect("install Codex fixture");
        let hooks_path = codex.join("hooks.json");
        let mut hooks = read_json(&hooks_path);
        let duplicate = hooks["hooks"][SESSION_START_EVENT][0]["hooks"][0].clone();
        hooks["hooks"]["PreToolUse"] = json!([{ "hooks": [duplicate] }]);
        fs::write(
            &hooks_path,
            serde_json::to_vec_pretty(&hooks).expect("serialize hooks with duplicate"),
        )
        .expect("write hooks with duplicate");

        let report = explicit_status(&codex, AgentKind::Codex);

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        let warning = report
            .warnings
            .iter()
            .find(|warning| warning.contains("unexpected event"))
            .expect("unexpected event warning");
        assert!(warning.contains("Codex SessionStart"), "{warning}");
        assert!(
            warning.contains("pohunek integration install --agent codex"),
            "{warning}"
        );
    }

    #[test]
    fn status_detects_broken_codex_registration_trust_and_read_errors() {
        for failure in ["registration", "trust", "read"] {
            let codex = temp_dir(&format!("status-codex-{failure}"));
            install_codex(&codex).expect("install Codex fixture");
            match failure {
                "registration" => {
                    let hooks_path = codex.join("hooks.json");
                    let mut hooks = read_json(&hooks_path);
                    hooks["hooks"][SESSION_START_EVENT][0]["hooks"][0]["timeout"] = json!(1);
                    fs::write(
                        hooks_path,
                        serde_json::to_vec_pretty(&hooks).expect("serialize modified hooks"),
                    )
                    .expect("write modified hooks");
                }
                "trust" => {
                    let config_path = codex.join("config.toml");
                    let config = fs::read_to_string(&config_path).expect("read Codex config");
                    fs::write(
                        config_path,
                        config.replacen("sha256:", "sha256:modified-", 1),
                    )
                    .expect("modify trust hash");
                }
                "read" => {
                    let hooks_path = codex.join("hooks.json");
                    fs::remove_file(&hooks_path).expect("remove hooks file");
                    fs::create_dir(&hooks_path).expect("replace hooks file with directory");
                }
                other => panic!("unknown fixture failure: {other}"),
            }

            let report = explicit_status(&codex, AgentKind::Codex);

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{failure}"
            );
            assert!(!report.warnings.is_empty(), "{failure}");
        }
    }

    #[test]
    fn aggregate_status_degrades_one_resolution_failure_without_aborting() {
        let codex = temp_dir("status-aggregate-resolve");
        install_codex(&codex).expect("install Codex fixture");

        let result = with_status_env(None, Some(&codex), None, || {
            super::status(protocol::IntegrationStatusParams { agent: None })
        })
        .expect("aggregate status");

        assert_eq!(result.agents.len(), 2);
        assert_eq!(result.agents[0].agent, AgentKind::Claude);
        assert_eq!(
            result.agents[0].state,
            protocol::IntegrationInstallState::Outdated
        );
        assert!(result.agents[0].warnings[0].contains("missing_env"));
        assert_eq!(result.agents[1].agent, AgentKind::Codex);
        assert_eq!(
            result.agents[1].state,
            protocol::IntegrationInstallState::Current
        );
    }

    #[test]
    fn explicit_status_degrades_resolution_failure_to_warning() {
        let report = with_status_env(None, None, None, || {
            super::status(protocol::IntegrationStatusParams {
                agent: Some(AgentKind::Claude),
            })
        })
        .expect("explicit status")
        .agents
        .pop()
        .expect("Claude report");

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert!(report.warnings[0].contains("missing_env"));
    }

    #[test]
    fn explicit_unsupported_status_agents_return_typed_errors() {
        for agent in [AgentKind::Shell, AgentKind::Hermes] {
            let error = super::status(protocol::IntegrationStatusParams { agent: Some(agent) })
                .expect_err("unsupported agent must fail");
            assert_eq!(error.code, "agent_not_installable");
        }
    }

    /// Serializes the process-global config-dir overrides used by status tests.
    static STATUS_CONFIG_LOCK: Mutex<()> = Mutex::new(());

    /// Restores the pre-test environment when the scoped assertion finishes.
    struct ConfigDirGuard {
        claude_dir: Option<std::ffi::OsString>,
        codex_dir: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    }

    impl Drop for ConfigDirGuard {
        fn drop(&mut self) {
            if let Some(value) = self.claude_dir.take() {
                std::env::set_var(super::CLAUDE_CONFIG_DIR_ENV, value);
            } else {
                std::env::remove_var(super::CLAUDE_CONFIG_DIR_ENV);
            }
            if let Some(value) = self.codex_dir.take() {
                std::env::set_var(super::CODEX_HOME_ENV, value);
            } else {
                std::env::remove_var(super::CODEX_HOME_ENV);
            }
            if let Some(value) = self.home.take() {
                std::env::set_var("HOME", value);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    fn with_config_dirs<T>(
        claude_dir: &Path,
        codex_dir: &Path,
        operation: impl FnOnce() -> T,
    ) -> T {
        let home = std::env::var_os("HOME");
        with_status_env(
            Some(claude_dir),
            Some(codex_dir),
            home.as_deref().map(Path::new),
            operation,
        )
    }

    fn with_status_env<T>(
        claude_dir: Option<&Path>,
        codex_dir: Option<&Path>,
        home: Option<&Path>,
        operation: impl FnOnce() -> T,
    ) -> T {
        let _lock = STATUS_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = ConfigDirGuard {
            claude_dir: std::env::var_os(super::CLAUDE_CONFIG_DIR_ENV),
            codex_dir: std::env::var_os(super::CODEX_HOME_ENV),
            home: std::env::var_os("HOME"),
        };
        set_optional_env(super::CLAUDE_CONFIG_DIR_ENV, claude_dir);
        set_optional_env(super::CODEX_HOME_ENV, codex_dir);
        set_optional_env("HOME", home);
        operation()
    }

    fn set_optional_env(key: &str, value: Option<&Path>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    fn explicit_status(config_dir: &Path, agent: AgentKind) -> protocol::IntegrationAgentStatus {
        with_config_dirs(config_dir, config_dir, || {
            super::status(protocol::IntegrationStatusParams { agent: Some(agent) })
        })
        .expect("explicit status")
        .agents
        .into_iter()
        .next()
        .expect("one agent report")
    }

    fn tree_snapshot(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
        fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, u32, Vec<u8>)>) {
            use std::os::unix::fs::PermissionsExt;

            let mut children = fs::read_dir(path)
                .expect("read snapshot directory")
                .map(|entry| entry.expect("read snapshot entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                let metadata = fs::symlink_metadata(&child).expect("snapshot metadata");
                let relative = child.strip_prefix(root).expect("relative snapshot path");
                let content = if metadata.is_file() {
                    fs::read(&child).expect("snapshot file")
                } else {
                    Vec::new()
                };
                entries.push((
                    relative.to_path_buf(),
                    metadata.permissions().mode(),
                    content,
                ));
                if metadata.is_dir() {
                    visit(root, &child, entries);
                }
            }
        }

        let mut entries = Vec::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn install_claude_preserves_unrelated_hooks_and_is_idempotent() {
        let claude_dir = temp_dir("claude-merge");
        // Pre-existing, unrelated user settings and hooks.
        let settings_path = claude_dir.join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "model": "claude-opus-4-8",
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-pretool" }
                        ]}
                    ],
                    "SessionStart": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-sessionstart" }
                        ]}
                    ],
                    "SessionEnd": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-sessionend" }
                        ]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_claude(&claude_dir).expect("first install");
        install_claude(&claude_dir).expect("reinstall");

        let settings = read_json(&settings_path);
        // Unrelated top-level key preserved.
        assert_eq!(settings["model"], json!("claude-opus-4-8"));
        // Unrelated PreToolUse hook preserved.
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            json!("echo user-pretool")
        );
        // Both the user's SessionStart hook and exactly one of ours survive
        // (no duplicate from the reinstall).
        let commands = session_start_command_hooks(&settings);
        assert!(commands.contains(&"echo user-sessionstart".to_owned()));
        let ours = commands
            .iter()
            .filter(|command| command.contains("pohunek-agent-state.sh"))
            .count();
        assert_eq!(
            ours, 1,
            "reinstall must not duplicate our hook: {commands:?}"
        );

        let release_commands = command_hooks(&settings, CLAUDE_SESSION_END_EVENT);
        assert!(release_commands.contains(&"echo user-sessionend".to_owned()));
        let release_ours = release_commands
            .iter()
            .filter(|command| command.contains("pohunek-agent-state.sh"))
            .count();
        assert_eq!(
            release_ours, 1,
            "reinstall must not duplicate our release hook: {release_commands:?}"
        );
    }

    #[test]
    fn install_codex_writes_hook_hooks_json_and_enables_feature() {
        let codex_dir = temp_dir("codex-fresh");
        let paths = install_codex(&codex_dir).expect("install codex");

        assert!(paths.hook_path.is_file());
        let hooks = read_json(&codex_dir.join("hooks.json"));
        let commands = session_start_command_hooks(&hooks);
        assert_eq!(commands.len(), 1);
        // Codex SessionStart hook carries no matcher key.
        assert!(hooks["hooks"]["SessionStart"][0].get("matcher").is_none());

        let config = fs::read_to_string(codex_dir.join("config.toml")).expect("config.toml");
        assert!(config.contains("[features]"), "config: {config}");
        assert!(config.contains("hooks = true"), "config: {config}");

        let trust_key = codex_hook_trust_key(
            &codex_dir.join("hooks.json"),
            CODEX_SESSION_START_TRUST_EVENT,
            0,
            0,
        );
        let trusted_hash = codex_command_hook_trusted_hash(
            CODEX_SESSION_START_TRUST_EVENT,
            &commands[0],
            HOOK_TIMEOUT_SECS,
            None,
        )
        .expect("hash installed Codex hook");
        assert!(
            config.contains(&format!("[hooks.state.{}]", toml_basic_string(&trust_key))),
            "config: {config}"
        );
        assert!(
            config.contains(&format!(
                "trusted_hash = {}",
                toml_basic_string(&trusted_hash)
            )),
            "config: {config}"
        );
    }

    #[test]
    fn install_codex_is_idempotent_in_config_toml() {
        let codex_dir = temp_dir("codex-idem");
        // Pre-existing config with an unrelated key.
        fs::write(
            codex_dir.join("config.toml"),
            "model = \"gpt-5\"\n\n[features]\nother = true\n",
        )
        .unwrap();

        install_codex(&codex_dir).expect("first install");
        let after_first = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        install_codex(&codex_dir).expect("reinstall");
        let after_second = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

        assert_eq!(after_first, after_second, "config.toml must be idempotent");
        assert!(after_second.contains("model = \"gpt-5\""), "{after_second}");
        assert!(after_second.contains("other = true"), "{after_second}");
        assert_eq!(
            after_second.matches("hooks = true").count(),
            1,
            "exactly one hooks=true: {after_second}"
        );
    }

    #[test]
    fn install_codex_updates_dotted_feature_config_toml() {
        let codex_dir = temp_dir("codex-dotted");
        fs::write(
            codex_dir.join("config.toml"),
            "model = \"gpt-5\"\nfeatures.hooks = false\n",
        )
        .unwrap();

        install_codex(&codex_dir).expect("install codex");
        let updated = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

        assert!(updated.contains("model = \"gpt-5\""), "{updated}");
        assert!(
            updated.contains("hooks = true") || updated.contains("features.hooks = true"),
            "{updated}"
        );
        assert!(updated.contains("trusted_hash"), "{updated}");
    }

    #[test]
    fn install_codex_fails_closed_on_inline_feature_table() {
        let codex_dir = temp_dir("codex-inline");
        fs::write(
            codex_dir.join("config.toml"),
            "features = { hooks = false }\n",
        )
        .unwrap();

        let err = install_codex(&codex_dir).expect_err("inline features table is refused");

        assert_eq!(err.code, "integration_settings_invalid");
        assert!(
            err.msg.contains("features") && err.msg.contains("not a TOML table"),
            "{err:?}"
        );
    }

    #[test]
    fn install_claude_preserves_user_hook_that_mentions_managed_notify_path() {
        let claude_dir = temp_dir("claude-substring-owned");
        let notify_path = claude_dir.join("hooks/pohunek-agent-notify.sh");
        let user_command = format!(
            "test -x {} && echo ok",
            shell_single_quote(&notify_path.display().to_string())
        );
        let settings_path = claude_dir.join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Notification": [
                        { "matcher": "permission_prompt", "hooks": [
                            { "type": "command", "command": user_command }
                        ]}
                    ]
                }
            }))
            .expect("serialize settings"),
        )
        .expect("write settings");

        install_claude(&claude_dir).expect("install claude");
        install_claude(&claude_dir).expect("reinstall claude");

        let settings = read_json(&settings_path);
        let commands = matcher_commands(&settings, "Notification", "permission_prompt");
        assert!(
            commands.contains(&user_command),
            "user hook mentioning the managed path must survive reinstall: {commands:?}"
        );
        let managed_command = format!(
            "sh {} notification permission_prompt",
            shell_single_quote(&notify_path.display().to_string())
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| *command == &managed_command)
                .count(),
            1,
            "genuine managed hook must remain idempotent: {commands:?}"
        );
    }

    #[test]
    fn install_codex_preserves_user_hook_that_mentions_managed_notify_path() {
        let codex_dir = temp_dir("codex-substring-owned");
        let notify_path = codex_dir.join("pohunek-agent-notify.sh");
        let user_command = format!(
            "test -x {} && echo ok",
            shell_single_quote(&notify_path.display().to_string())
        );
        let hooks_path = codex_dir.join("hooks.json");
        fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PermissionRequest": [
                        { "hooks": [
                            { "type": "command", "command": user_command }
                        ]}
                    ]
                }
            }))
            .expect("serialize hooks"),
        )
        .expect("write hooks");

        install_codex(&codex_dir).expect("install codex");
        install_codex(&codex_dir).expect("reinstall codex");

        let hooks = read_json(&hooks_path);
        let commands = command_hooks(&hooks, "PermissionRequest");
        assert!(
            commands.contains(&user_command),
            "user hook mentioning the managed path must survive reinstall: {commands:?}"
        );
        let managed_command = hook_command(&notify_path, "permission_request");
        assert_eq!(
            commands
                .iter()
                .filter(|command| *command == &managed_command)
                .count(),
            1,
            "genuine managed hook must remain idempotent: {commands:?}"
        );
    }

    #[test]
    fn install_claude_writes_notification_hook_and_modern_events() {
        let claude_dir = temp_dir("claude-notify-install");
        let settings_path = claude_dir.join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Notification": [
                        { "matcher": "permission_prompt", "hooks": [
                            { "type": "command", "command": "echo user-notification" }
                        ]}
                    ],
                    "Stop": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-stop" }
                        ]}
                    ],
                    "StopFailure": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-stop-failure" }
                        ]}
                    ]
                }
            }))
            .expect("serialize settings"),
        )
        .expect("write settings");
        let paths = install_claude(&claude_dir).expect("install claude");
        install_claude(&claude_dir).expect("reinstall claude");
        let notify_path = claude_dir.join("hooks/pohunek-agent-notify.sh");

        assert!(paths.hook_path.ends_with("pohunek-agent-state.sh"));
        assert!(notify_path.is_file(), "notification hook must be written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&notify_path)
                .expect("notify hook metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "notify hook must be executable");
        };

        let settings = read_json(&settings_path);
        assert!(
            command_hooks(&settings, "Notification").contains(&"echo user-notification".to_owned())
        );
        assert!(command_hooks(&settings, "Stop").contains(&"echo user-stop".to_owned()));
        assert!(
            command_hooks(&settings, "StopFailure").contains(&"echo user-stop-failure".to_owned())
        );
        assert_eq!(
            command_hooks(&settings, "Notification")
                .iter()
                .filter(|command| command.contains("pohunek-agent-notify.sh"))
                .count(),
            5,
            "reinstall must keep one notification hook per matcher"
        );
        for matcher in [
            "permission_prompt",
            "elicitation_dialog",
            "auth_success",
            "elicitation_complete",
            "elicitation_response",
        ] {
            let commands = matcher_commands(&settings, "Notification", matcher);
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command.contains("pohunek-agent-notify.sh"))
                    .count(),
                1,
                "one Pohunek Notification hook for {matcher}"
            );
            assert!(
                commands
                    .iter()
                    .any(|command| command.contains("pohunek-agent-notify.sh")),
                "Notification command for {matcher}: {commands:?}"
            );
        }
        for event in ["Stop", "StopFailure"] {
            let commands = command_hooks(&settings, event);
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command.contains("pohunek-agent-notify.sh"))
                    .count(),
                1,
                "one Pohunek {event} hook"
            );
            assert!(
                commands
                    .iter()
                    .any(|command| command.contains("pohunek-agent-notify.sh")),
                "{event} command: {commands:?}"
            );
        }
    }

    #[test]
    fn install_codex_writes_notification_hook_modern_events_and_trust_without_notify() {
        let codex_dir = temp_dir("codex-notify-install");
        let hooks_path = codex_dir.join("hooks.json");
        fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PermissionRequest": [
                        { "hooks": [
                            { "type": "command", "command": "echo user-permission" }
                        ]}
                    ],
                    "Stop": [
                        { "hooks": [
                            { "type": "command", "command": "echo user-stop" }
                        ]}
                    ]
                }
            }))
            .expect("serialize hooks"),
        )
        .expect("write hooks");
        let paths = install_codex(&codex_dir).expect("install codex");
        install_codex(&codex_dir).expect("reinstall codex");
        let notify_path = codex_dir.join("pohunek-agent-notify.sh");

        assert!(paths.hook_path.ends_with("pohunek-agent-state.sh"));
        assert!(notify_path.is_file(), "notification hook must be written");
        let hooks = read_json(&hooks_path);
        assert!(
            hooks.get("notify").is_none(),
            "Codex approval notifications must not use legacy notify: {hooks}"
        );
        assert!(
            command_hooks(&hooks, "PermissionRequest").contains(&"echo user-permission".to_owned())
        );
        assert!(command_hooks(&hooks, "Stop").contains(&"echo user-stop".to_owned()));
        for event in ["PermissionRequest", "Stop"] {
            let commands = command_hooks(&hooks, event);
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command.contains("pohunek-agent-notify.sh"))
                    .count(),
                1,
                "one Pohunek {event} hook after reinstall"
            );
            assert!(
                commands
                    .iter()
                    .any(|command| command.contains("pohunek-agent-notify.sh")),
                "{event} command: {commands:?}"
            );
        }

        let config = fs::read_to_string(codex_dir.join("config.toml")).expect("config.toml");
        for event in ["session_start", "permission_request", "stop"] {
            assert!(
                config.contains(&format!(
                    "{}:{event}:",
                    codex_dir.join("hooks.json").display()
                )),
                "missing trust metadata for {event}: {config}"
            );
        }
    }

    #[test]
    fn claude_notification_hook_maps_matchers() {
        for (matcher, kind, severity, dedupe_key) in [
            (
                "permission_prompt",
                "approval_required",
                "action_required",
                Some("attention:session-123"),
            ),
            (
                "elicitation_dialog",
                "approval_required",
                "action_required",
                Some("attention:session-123"),
            ),
            ("auth_success", "system", "success", None),
            ("elicitation_complete", "system", "info", None),
            ("elicitation_response", "system", "info", None),
        ] {
            let (_stdout, _stderr, request) = captured_notification_request(
                "claude",
                &["notification", matcher],
                &json!({"hook_event_id": format!("evt-{matcher}")}),
            );
            assert_notification_payload(
                &request,
                "claude",
                &format!("Notification.{matcher}"),
                kind,
                severity,
                Some(matcher),
                dedupe_key,
            );
        }
    }

    #[test]
    fn claude_idle_prompt_does_not_create_an_attention_notification() {
        let (status, stdout, stderr, requests) = run_notification_asset(
            "claude",
            &["notification", "idle_prompt"],
            &json!({"hook_event_id": "evt-idle"}),
            true,
        );

        assert!(status.success(), "hook exited with {status}: {stderr}");
        assert!(stdout.is_empty(), "hook must not print stdout");
        assert!(stderr.is_empty(), "hook must not print stderr");
        assert!(requests.is_empty(), "idle prompt must remain quiet");
    }

    #[test]
    fn claude_notification_hook_maps_stop_events() {
        for (action, provider_event, kind, severity, dedupe_key) in [
            (
                "stop",
                "Stop",
                "turn_completed",
                "info",
                Some("turn:session-123"),
            ),
            ("stop_failure", "StopFailure", "error", "error", None),
        ] {
            let (_stdout, _stderr, request) = captured_notification_request(
                "claude",
                &[action],
                &json!({"hook_event_id": format!("evt-{action}")}),
            );
            assert_notification_payload(
                &request,
                "claude",
                provider_event,
                kind,
                severity,
                None,
                dedupe_key,
            );
        }
    }

    #[test]
    fn codex_notification_hook_maps_lifecycle_events() {
        for (action, provider_event, kind, severity, dedupe_key) in [
            (
                "permission_request",
                "PermissionRequest",
                "approval_required",
                "action_required",
                Some("attention:session-123"),
            ),
            (
                "stop",
                "Stop",
                "turn_completed",
                "info",
                Some("turn:session-123"),
            ),
        ] {
            let (_stdout, _stderr, request) = captured_notification_request(
                "codex",
                &[action],
                &json!({"hook_event_id": format!("evt-{action}")}),
            );
            assert_notification_payload(
                &request,
                "codex",
                provider_event,
                kind,
                severity,
                None,
                dedupe_key,
            );
        }
    }

    #[test]
    fn notification_hooks_omit_raw_payload_and_environment() {
        let sentinels = [
            "DROP_ME_PROMPT",
            "DROP_ME_OUTPUT",
            "DROP_ME_ENV",
            "DROP_ME_TOOL",
            "DROP_ME_CWD",
        ];
        let input = json!({
            "hook_event_id": "evt-safe-1",
            "prompt": "DROP_ME_PROMPT",
            "terminal_output": "DROP_ME_OUTPUT",
            "env": {"TOKEN": "DROP_ME_ENV"},
            "tool_result": "DROP_ME_TOOL",
            "cwd": "DROP_ME_CWD"
        });
        let (stdout, stderr, request) =
            captured_notification_request("codex", &["permission_request"], &input);

        let request_text = request.to_string();
        for sentinel in sentinels {
            assert!(
                !request_text.contains(sentinel),
                "raw payload leaked into notification request: {request_text}"
            );
            assert!(!stdout.contains(sentinel), "raw payload leaked to stdout");
            assert!(!stderr.contains(sentinel), "raw payload leaked to stderr");
        }
    }

    #[test]
    fn notification_hooks_exit_zero_when_socket_unavailable() {
        for (agent, args) in [
            ("codex", vec!["permission_request"]),
            ("claude", vec!["notification", "permission_prompt"]),
        ] {
            let (status, stdout, stderr, requests) =
                run_notification_asset(agent, &args, &json!({}), false);
            assert!(
                status.success(),
                "{agent} hook exited with {status}: {stderr}"
            );
            assert_eq!(stdout, "", "{agent} hook must not print stdout");
            assert_eq!(stderr, "", "{agent} hook must not print stderr");
            assert!(
                requests.is_empty(),
                "socket unavailable captured no requests"
            );
        }
    }

    #[test]
    fn install_into_missing_dir_fails_fast() {
        let missing = temp_dir("missing-parent").join("does-not-exist");
        let err = install_claude(&missing).expect_err("missing claude dir");
        assert_eq!(err.code, "agent_config_dir_missing");
        let err = install_codex(&missing).expect_err("missing codex dir");
        assert_eq!(err.code, "agent_config_dir_missing");
    }
}
