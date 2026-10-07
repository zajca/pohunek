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

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use pohunek_worker_protocol::HookAction;
use protocol::{
    ErrorClass, IntegrationAgentStatus, IntegrationInstallReport, IntegrationInstallState,
    IntegrationRecovery, ProtocolError, RuntimeId, RuntimeRef, EXPECTED_INTEGRATION_VERSION,
};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::{value, DocumentMut, Item, Table};

#[cfg(unix)]
use pohunek_platform::filesystem::{
    EntryKind, FsError, LockKind, TrustedDir as PlatformTrustedDir,
};

#[cfg(unix)]
mod commit;
#[cfg(unix)]
mod doctor;
mod handler;
mod homes;
mod provider_handlers;
mod reporter;
use reporter::ReporterIdentity;
#[cfg(unix)]
mod quarantine;
#[cfg(unix)]
mod uninstall;
#[cfg(all(test, unix))]
pub use doctor::doctor;
#[cfg(unix)]
pub use doctor::doctor_in;
pub use handler::{
    install_in, status_in, AssetManifest, DaemonHandler, Handler, RetainedSchemas, StagedUpdate,
};
#[cfg(all(test, unix))]
pub use homes::process_environment::uninstall_for;
#[cfg(test)]
pub use homes::process_environment::{
    config_homes_for_tests, install_for, install_for_retained, status_for,
};
pub use homes::{ConfigHomes, HomeSelection};
pub(crate) use homes::{Home, LaunchHome};
#[cfg(all(test, unix))]
pub use uninstall::uninstall;
#[cfg(unix)]
pub use uninstall::{uninstall_claude, uninstall_codex, uninstall_in};
#[cfg(all(test, unix))]
mod handler_tests;
#[cfg(all(test, unix))]
mod hook_pid_tests;
#[cfg(all(test, unix))]
mod lifecycle_tests;
#[cfg(all(test, unix))]
mod removal_tests;
#[cfg(all(test, unix))]
mod worker_hook_tests;
#[cfg(unix)]
use commit::{Action, Expected, Step, StepGate};

// The agent-handshake env var names are defined once in `protocol` (the shared
// contract crate) so the daemon (which injects them), the installed hook (which
// reads them), and the CLI (which reads `ENV_SESSION_ID` for the
// self-feeding-attach guard) cannot drift. Re-exported here so existing daemon
// call sites and tests keep referring to `integration::ENV_*` unchanged.
pub use protocol::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};

/// Path of a runnable copy of `script` for `agent` under test.
///
/// The Codex scripts exist only as rendered text, so their bytes are written
/// into a directory scoped to the calling test thread; the other agents' scripts
/// are run from the source tree.
#[cfg(test)]
pub(crate) fn runnable_script(agent: &str, script: &str) -> PathBuf {
    let source = pohunek_test_support::manifest_dir()
        .join("src/integration/assets")
        .join(agent)
        .join(script);
    if agent != RuntimeId::CODEX {
        return source;
    }
    let body = match script {
        STATE_HOOK_INSTALL_NAME => CODEX_HOOK_ASSET.as_str(),
        NOTIFY_HOOK_INSTALL_NAME => CODEX_NOTIFY_HOOK_ASSET.as_str(),
        other => panic!("no rendered Codex script named {other}"),
    };
    let path = crate::test_support::thread_scoped_dir("pohunek-rendered-script-").join(script);
    fs::write(&path, body).expect("write the rendered script");
    path
}

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
/// The state hook template, embedded at compile time.
const CODEX_HOOK_TEMPLATE: &str = include_str!("assets/codex/pohunek-agent-state.sh");
/// The notification hook template, embedded at compile time.
const CODEX_NOTIFY_HOOK_TEMPLATE: &str = include_str!("assets/codex/pohunek-agent-notify.sh");
/// Display name of the runtime the Codex handler's scripts are rendered for.
const CODEX_REPORTER_NAME: &str = "Codex";
/// The runtime values the Codex handler renders its templates with.
static CODEX_REPORTER: LazyLock<ReporterIdentity> = LazyLock::new(|| {
    ReporterIdentity::new(RuntimeId::CODEX, CODEX_REPORTER_NAME)
        .expect("the Codex reporter identity is a valid constant")
});
/// The Codex hook script rendered for the Codex runtime; its bytes are what is
/// installed and what drift is measured against.
static CODEX_HOOK_ASSET: LazyLock<String> = LazyLock::new(|| {
    CODEX_REPORTER
        .render(CODEX_HOOK_TEMPLATE)
        .expect("the Codex state template renders")
});
/// The Codex notification hook script rendered for the Codex runtime.
static CODEX_NOTIFY_HOOK_ASSET: LazyLock<String> = LazyLock::new(|| {
    CODEX_REPORTER
        .render(CODEX_NOTIFY_HOOK_TEMPLATE)
        .expect("the Codex notification template renders")
});
/// Sanitized payloads matching the upstream Codex subagent hook schema.
#[cfg(test)]
const CODEX_SUBAGENT_COMPATIBILITY_FIXTURE: &str =
    include_str!("../../../../compat/codex/subagent-hooks.json");
/// Managed hook assets are currently below 8 KiB; 64 KiB leaves ample growth
/// while bounding status memory and I/O for installer-owned executable files.
const MANAGED_ASSET_INSPECTION_LIMIT_BYTES: usize = 64 * 1024;
/// Provider configuration can legitimately contain unrelated user settings, so
/// status allows 2 MiB before requiring manual inspection rather than parsing
/// an attacker-controlled or accidentally expanded file in full.
const PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// One byte beyond a limit distinguishes an exact-boundary file from overflow.
const INSPECTION_LIMIT_PROBE_BYTES: usize = 1;
/// Unix permission and special-mode bits relevant to executable trust checks.
#[cfg(unix)]
const UNIX_MODE_MASK: u32 = 0o7777;
/// Group/other write bits violate the owner-private executable path boundary.
#[cfg(unix)]
const GROUP_OR_OTHER_WRITE_MASK: u32 = 0o022;
/// Per-hook timeout (seconds) recorded in the agent's hook config. The agent
/// kills a hook that outlives it, so it is the longest a hook can take to
/// deliver a report.
pub(crate) const HOOK_TIMEOUT_SECS: u64 = 10;
/// Action argument passed to the hook script for the `SessionStart` event.
const HOOK_ACTION: &str = "session";
/// Action argument passed to the hook script for the `SessionEnd` event.
const HOOK_RELEASE_ACTION: &str = "release";
/// Action argument passed to the hook script when a provider subagent starts.
const SUBAGENT_START_ACTION: &str = "subagent-start";
/// Action argument passed to the hook script when a provider subagent stops.
const SUBAGENT_STOP_ACTION: &str = "subagent-stop";
/// `SessionStart` event name in the agents' hook config.
const SESSION_START_EVENT: &str = "SessionStart";
/// `SessionEnd` event name in Claude's hook config.
const SESSION_END_EVENT: &str = "SessionEnd";
/// Provider lifecycle event fired when a subagent starts.
const SUBAGENT_START_EVENT: &str = "SubagentStart";
/// Provider lifecycle event fired when a subagent stops.
const SUBAGENT_STOP_EVENT: &str = "SubagentStop";
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
/// Codex trust identity name for `SubagentStart`.
const CODEX_SUBAGENT_START_TRUST_EVENT: &str = "subagent_start";
/// Codex trust identity name for `SubagentStop`.
const CODEX_SUBAGENT_STOP_TRUST_EVENT: &str = "subagent_stop";
/// Codex trust keys owned by this installer across all managed lifecycle hooks.
const CODEX_MANAGED_TRUST_EVENTS: &[&str] = &[
    CODEX_SESSION_START_TRUST_EVENT,
    CODEX_PERMISSION_REQUEST_TRUST_EVENT,
    CODEX_STOP_TRUST_EVENT,
    CODEX_SUBAGENT_START_TRUST_EVENT,
    CODEX_SUBAGENT_STOP_TRUST_EVENT,
];
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
/// Newly created Claude hook directories are private to the daemon owner.
#[cfg(unix)]
const MANAGED_HOOK_DIR_MODE: u32 = 0o700;
/// Newly created provider registration files are readable only by the owner.
#[cfg(unix)]
const NEW_REGISTRATION_MODE: u32 = 0o600;
/// Lock file inside an agent config directory that serializes installers.
#[cfg(unix)]
const INSTALL_LOCK_NAME: &str = ".pohunek-integration.lock";
/// Quarantine prefix for a rolled-back directory the installer had created.
#[cfg(unix)]
const CREATED_DIR_QUARANTINE_PREFIX: &str = ".pohunek-integration-created-";
/// Owner-private mode of the installer lock file.
#[cfg(unix)]
const INSTALL_LOCK_MODE: u32 = 0o600;
/// Error code for an install that found another installer holding the lock.
#[cfg(unix)]
const INSTALL_IN_PROGRESS_CODE: &str = "integration_install_in_progress";

/// Hook operations the managed Claude asset set reports.
const CLAUDE_REPORTED_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::IdentityRelease,
    HookAction::SubagentStart,
    HookAction::SubagentStop,
    HookAction::Notification,
];
/// Hook operations the managed Codex asset set reports. Codex has no session
/// end hook, so it never reports an identity release.
const CODEX_REPORTED_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::SubagentStart,
    HookAction::SubagentStop,
    HookAction::Notification,
];

/// Env var the tests set to relocate Claude's config dir.
#[cfg(test)]
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Env var the tests set to relocate Codex's config dir.
#[cfg(test)]
pub(crate) const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Files the installer wrote for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// Absolute path of the installed hook script.
    pub hook_path: PathBuf,
    /// Config files created or merged into, in the order touched.
    pub config_paths: Vec<PathBuf>,
    /// Quarantined originals whose deletion did not finish after the install
    /// committed; empty when cleanup completed.
    pub cleanup_incomplete: Vec<String>,
}

/// Install the hooks for the selected runtime(s) through their integration
/// handlers.
///
/// `Some(agent)` installs the handler its runtime definition names and fails
/// fast if that handler's config dir is absent. `None` installs every
/// daemon-managed handler whose config dir exists, and errors only if none are
/// present.
///
/// # Errors
///
/// `agent_not_installable` for a runtime without a daemon-run handler,
/// `agent_config_dir_missing` when a requested (or, for `None`, every) config
/// dir is absent, `integration_update_incompatible` when the new asset set is
/// not compatible with the active one, or any underlying I/O / settings error.
#[cfg(test)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the owned argument is the established test entry-point shape"
)]
pub fn install(
    agent: Option<RuntimeRef>,
) -> Result<protocol::IntegrationInstallResult, ProtocolError> {
    install_for(&crate::agent::host::fixture::builtin_host(), agent.as_ref())
}

/// Inspect the managed hook files of the selected runtime(s) without writing
/// anything.
///
/// Runtimes without a daemon-run handler are rejected so callers cannot
/// mistake an empty report for "nothing is installed". Missing config
/// directories are reported as unavailable rather than treated as errors.
///
/// # Errors
///
/// Returns [`ProtocolError`] for runtimes without a daemon-run handler.
#[cfg(test)]
pub fn status(
    params: protocol::IntegrationStatusParams,
) -> Result<protocol::IntegrationStatusResult, ProtocolError> {
    status_for(&crate::agent::host::fixture::builtin_host(), params)
}

/// The session warning for a launch of `agent` whose managed hook assets are
/// installed but differ from the ones this daemon embeds.
///
/// `profile` selects the config home the launch uses. The result is `None` when
/// the integration is not installed, is current, or cannot be inspected: a
/// launch is never refused or delayed by this diagnostic, and the doctor reports
/// the cases it cannot name.
#[must_use]
pub fn outdated_assets_warning(
    homes: &ConfigHomes,
    agent: RuntimeRef,
    profile: Option<String>,
) -> Option<protocol::SessionWarning> {
    let label = agent.as_wire().to_owned();
    let params = protocol::IntegrationStatusParams {
        agent: Some(agent),
        profile: profile.clone(),
        all_profiles: false,
    };
    let status = status_in(homes, params).ok()?.agents.into_iter().next()?;
    if !status.available || status.state != IntegrationInstallState::Outdated {
        return None;
    }
    let scope = profile.map_or_else(String::new, |name| format!(" --profile {name}"));
    Some(protocol::SessionWarning {
        kind: protocol::SessionWarningKind::Hook,
        message: format!(
            "the installed {label} hook assets do not match this daemon, so the agent's hooks may report nothing: run `pohunek integration install --agent {label}{scope}` on the daemon host"
        ),
        detail: (!status.warnings.is_empty()).then(|| status.warnings.join("; ")),
    })
}

/// Degrade one supported agent's config-resolution failure into a warning.
fn reported_agent_status(
    agent: StatusAgent,
    home: Result<&Path, &ProtocolError>,
) -> IntegrationAgentStatus {
    match home {
        Ok(dir) => agent_status_at(agent, dir),
        Err(error) => IntegrationAgentStatus {
            agent: agent.kind(),
            available: false,
            expected_asset_paths: Vec::new(),
            present_asset_paths: Vec::new(),
            registration_paths: Vec::new(),
            installed_version: None,
            expected_version: EXPECTED_INTEGRATION_VERSION,
            state: IntegrationInstallState::Outdated,
            recovery: IntegrationRecovery::RepairConfiguration,
            warnings: vec![format!(
                "agent config directory could not be resolved ({})",
                error.code
            )],
            home: None,
        },
    }
}

/// The provider whose managed asset set an inspection or install operates on.
///
/// Only the Claude and Codex handlers use it, to parameterize their shared
/// inspection code; dispatch between runtimes goes through the handler
/// registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusAgent {
    Claude,
    Codex,
}

impl StatusAgent {
    fn kind(self) -> RuntimeRef {
        match self {
            Self::Claude => RuntimeRef::claude(),
            Self::Codex => RuntimeRef::codex(),
        }
    }

    /// Recovery hint for a missing provider config directory.
    fn missing_config_hint(self) -> &'static str {
        match self {
            Self::Claude => "install Claude Code first",
            Self::Codex => "install Codex first",
        }
    }
}

/// Build one supported agent's read-only status report for the config
/// directory `dir`.
fn agent_status_at(agent: StatusAgent, dir: &Path) -> IntegrationAgentStatus {
    match agent {
        StatusAgent::Claude => status_at(
            StatusAgent::Claude,
            dir,
            CLAUDE_HOOK_ASSET,
            CLAUDE_NOTIFY_HOOK_ASSET,
        ),
        StatusAgent::Codex => status_at(
            StatusAgent::Codex,
            dir,
            CODEX_HOOK_ASSET.as_str(),
            CODEX_NOTIFY_HOOK_ASSET.as_str(),
        ),
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

    if let Some(issue) = config_dir_issue(config_dir) {
        return IntegrationAgentStatus {
            agent: agent.kind(),
            available: false,
            expected_asset_paths,
            present_asset_paths: Vec::new(),
            registration_paths: registration_path_strings,
            installed_version: None,
            expected_version: EXPECTED_INTEGRATION_VERSION,
            state: issue.state,
            recovery: issue.recovery,
            warnings: issue.warning.map(str::to_owned).into_iter().collect(),
            home: None,
        };
    }

    let mut warnings = Vec::new();
    let mut recovery = IntegrationRecovery::None;
    let parent_chain_current = managed_asset_parent_chain_current(
        agent,
        config_dir,
        state_path
            .parent()
            .expect("managed asset path always has a parent"),
        &mut warnings,
        &mut recovery,
    );
    let assets = [
        inspect_asset(
            agent,
            ManagedAssetKind::StateHook,
            state_path,
            state_asset,
            parent_chain_current,
            &mut warnings,
            &mut recovery,
        ),
        inspect_asset(
            agent,
            ManagedAssetKind::NotificationHook,
            notify_path,
            notify_asset,
            parent_chain_current,
            &mut warnings,
            &mut recovery,
        ),
    ];
    let (registration_footprint, registrations_current) = match agent {
        StatusAgent::Claude => inspect_claude_registration(
            config_dir,
            &assets[0].path,
            &assets[1].path,
            &mut warnings,
            &mut recovery,
        ),
        StatusAgent::Codex => inspect_codex_registration(
            config_dir,
            &assets[0].path,
            &assets[1].path,
            &mut warnings,
            &mut recovery,
        ),
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
        available: true,
        expected_asset_paths,
        present_asset_paths,
        registration_paths: registration_path_strings,
        installed_version,
        expected_version: EXPECTED_INTEGRATION_VERSION,
        state,
        recovery,
        warnings,
        home: None,
    }
}

/// Why an agent's config directory yields no inspectable install.
struct ConfigDirIssue {
    state: IntegrationInstallState,
    recovery: IntegrationRecovery,
    warning: Option<&'static str>,
}

/// What exists at an agent config path, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigPath {
    Absent,
    Directory,
    /// A symlink, live or dangling; the installer never follows one.
    Symlink,
    /// A file or another non-directory.
    Other,
    /// The path could not be inspected.
    Unreadable,
}

/// Classifies `path` component by component, never following a symlink.
///
/// A symlink anywhere on the path, live or dangling, is [`ConfigPath::Symlink`]
/// because the installer's trusted walk refuses it; a missing component is
/// [`ConfigPath::Absent`].
fn config_path_kind(path: &Path) -> ConfigPath {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match component {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => continue,
            std::path::Component::Normal(_) => {}
            std::path::Component::CurDir | std::path::Component::ParentDir => {
                return ConfigPath::Unreadable;
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return ConfigPath::Symlink,
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_metadata) => return ConfigPath::Other,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return ConfigPath::Absent,
            Err(_error) => return ConfigPath::Unreadable,
        }
    }
    ConfigPath::Directory
}

/// The error for an agent config path that is a symlink.
fn config_dir_is_symlink(path: &Path) -> ProtocolError {
    path_untrusted(
        path,
        "agent config path is a symlink; point the provider config directory at the canonical directory",
    )
}

/// Classifies the agent config directory.
///
/// An absent path means the optional agent is not installed on this host: it is
/// reported as `not_installed` with no warning and nothing to repair. A symlink
/// anywhere on the path (even a dangling one), a non-directory, a directory the
/// trusted walk refuses (foreign owner, group/world-writable), or an
/// uninspectable path is a failure that needs configuration repair.
fn config_dir_issue(config_dir: &Path) -> Option<ConfigDirIssue> {
    let failure = |warning| ConfigDirIssue {
        state: IntegrationInstallState::Outdated,
        recovery: IntegrationRecovery::RepairConfiguration,
        warning: Some(warning),
    };
    match config_path_kind(config_dir) {
        // The same descriptor-relative walk the installer performs, read-only,
        // so status and doctor refuse exactly the paths install refuses.
        ConfigPath::Directory => PlatformTrustedDir::open_absolute_owner_safe(
            config_dir,
            GROUP_OR_OTHER_WRITE_MASK,
        )
        .err()
        .map(|_error| {
            failure(
                "agent config directory failed trusted filesystem validation; repair its ownership and permissions",
            )
        }),
        ConfigPath::Symlink => Some(failure(
            "agent config path is a symlink; use the canonical directory",
        )),
        ConfigPath::Other => Some(failure("agent config path is not a directory")),
        ConfigPath::Absent => Some(ConfigDirIssue {
            state: IntegrationInstallState::NotInstalled,
            recovery: IntegrationRecovery::None,
            warning: None,
        }),
        ConfigPath::Unreadable => Some(failure("agent config directory could not be inspected")),
    }
}

fn require_recovery(current: &mut IntegrationRecovery, required: IntegrationRecovery) {
    if required == IntegrationRecovery::RepairConfiguration || *current == IntegrationRecovery::None
    {
        *current = required;
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

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnixMetadataSnapshot {
    is_expected_type: bool,
    owner_uid: u32,
    mode: u32,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataTrustIssue {
    UnexpectedType,
    ForeignOwner { actual: u32, expected: u32 },
    GroupOrOtherWritable { mode: u32 },
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentStatus {
    Current,
    Missing,
    Unsafe,
}

#[cfg(unix)]
fn evaluate_owner_private_metadata(
    metadata: UnixMetadataSnapshot,
    effective_uid: u32,
) -> Result<(), MetadataTrustIssue> {
    if !metadata.is_expected_type {
        return Err(MetadataTrustIssue::UnexpectedType);
    }
    if metadata.owner_uid != effective_uid {
        return Err(MetadataTrustIssue::ForeignOwner {
            actual: metadata.owner_uid,
            expected: effective_uid,
        });
    }
    if metadata.mode & GROUP_OR_OTHER_WRITE_MASK != 0 {
        return Err(MetadataTrustIssue::GroupOrOtherWritable {
            mode: metadata.mode,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_snapshot(metadata: &fs::Metadata, is_expected_type: bool) -> UnixMetadataSnapshot {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    UnixMetadataSnapshot {
        is_expected_type,
        owner_uid: metadata.uid(),
        mode: metadata.permissions().mode() & UNIX_MODE_MASK,
    }
}

#[cfg(unix)]
fn managed_asset_parent_chain_current(
    agent: StatusAgent,
    config_dir: &Path,
    asset_parent: &Path,
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
) -> bool {
    let relative_parent = match asset_parent.strip_prefix(config_dir) {
        Ok(relative_parent) => relative_parent,
        Err(_error) => {
            warnings.push("managed asset parent escapes the agent config root".to_owned());
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return false;
        }
    };
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    let mut parent = config_dir.to_path_buf();
    if inspect_managed_parent(agent, &parent, effective_uid, warnings) != ParentStatus::Current {
        require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
        return false;
    }
    for component in relative_parent.components() {
        parent.push(component);
        match inspect_managed_parent(agent, &parent, effective_uid, warnings) {
            ParentStatus::Current => {}
            ParentStatus::Missing => return true,
            ParentStatus::Unsafe => {
                require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
                return false;
            }
        }
    }
    true
}

#[cfg(not(unix))]
fn managed_asset_parent_chain_current(
    _agent: StatusAgent,
    _config_dir: &Path,
    _asset_parent: &Path,
    _warnings: &mut Vec<String>,
    _recovery: &mut IntegrationRecovery,
) -> bool {
    true
}

#[cfg(unix)]
fn inspect_managed_parent(
    agent: StatusAgent,
    path: &Path,
    effective_uid: u32,
    warnings: &mut Vec<String>,
) -> ParentStatus {
    let file = match open_inspection_directory(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ParentStatus::Missing,
        Err(_error) => {
            warnings.push(format!(
                "managed asset parent {} could not be opened safely",
                path.display()
            ));
            return ParentStatus::Unsafe;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_error) => {
            warnings.push(format!(
                "managed asset parent {} metadata could not be read",
                path.display()
            ));
            return ParentStatus::Unsafe;
        }
    };
    match evaluate_owner_private_metadata(
        metadata_snapshot(&metadata, metadata.file_type().is_dir()),
        effective_uid,
    ) {
        Ok(()) => ParentStatus::Current,
        Err(MetadataTrustIssue::UnexpectedType) => {
            warnings.push(format!(
                "managed asset parent {} is not a real directory",
                path.display()
            ));
            ParentStatus::Unsafe
        }
        Err(MetadataTrustIssue::ForeignOwner { actual, expected }) => {
            warnings.push(format!(
                "managed asset parent {} is owned by uid {actual}, not daemon uid {expected}; repair the agent configuration before running `pohunek integration install --agent {}`",
                path.display(),
                agent.kind().as_wire()
            ));
            ParentStatus::Unsafe
        }
        Err(MetadataTrustIssue::GroupOrOtherWritable { mode }) => {
            warnings.push(format!(
                "managed asset parent {} is unsafe (mode {mode:04o}; group or other users can replace managed hooks); repair the directory permissions before running `pohunek integration install --agent {}`",
                path.display(),
                agent.kind().as_wire()
            ));
            ParentStatus::Unsafe
        }
    }
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
    parent_chain_current: bool,
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
) -> AssetStatus {
    let description = kind.description();
    let mut file = match open_inspection_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warnings.push(format!("managed {description} is missing"));
            require_recovery(recovery, IntegrationRecovery::Reinstall);
            return AssetStatus {
                path,
                content: None,
                present: false,
                footprint: false,
                current: false,
            };
        }
        Err(_error) => {
            warnings.push(format!("managed {description} could not be opened safely"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return AssetStatus {
                path,
                content: None,
                present: false,
                footprint: true,
                current: false,
            };
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_error) => {
            warnings.push(format!("managed {description} metadata could not be read"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return AssetStatus {
                path,
                content: None,
                present: true,
                footprint: true,
                current: false,
            };
        }
    };
    let metadata_current = asset_metadata_current(agent, kind, &metadata, warnings, recovery);
    let content = match read_bounded_utf8_from_file(&mut file, MANAGED_ASSET_INSPECTION_LIMIT_BYTES)
    {
        Ok(BoundedRead::Content(content)) => content,
        Ok(BoundedRead::Oversized) => {
            warnings.push(format!(
                "managed {description} exceeds the {MANAGED_ASSET_INSPECTION_LIMIT_BYTES}-byte inspection limit; reinstall the integration to replace it"
            ));
            require_recovery(recovery, IntegrationRecovery::Reinstall);
            return AssetStatus {
                path,
                content: None,
                present: true,
                footprint: true,
                current: false,
            };
        }
        Err(_error) => {
            warnings.push(format!("managed {description} could not be read"));
            require_recovery(recovery, IntegrationRecovery::Reinstall);
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
        require_recovery(recovery, IntegrationRecovery::Reinstall);
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
    AssetStatus {
        path,
        content: Some(content),
        present: true,
        footprint: true,
        current: content_current && metadata_current && parent_chain_current,
    }
}

#[cfg(unix)]
fn asset_metadata_current(
    agent: StatusAgent,
    kind: ManagedAssetKind,
    metadata: &fs::Metadata,
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let effective_uid = nix::unistd::Uid::effective().as_raw();
    match evaluate_owner_private_metadata(
        metadata_snapshot(metadata, metadata.file_type().is_file()),
        effective_uid,
    ) {
        Ok(()) | Err(MetadataTrustIssue::GroupOrOtherWritable { .. }) => {}
        Err(MetadataTrustIssue::UnexpectedType) => {
            warnings.push(format!(
                "managed {} is not a regular file",
                kind.description()
            ));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return false;
        }
        Err(MetadataTrustIssue::ForeignOwner { actual, expected }) => {
            warnings.push(format!(
                "managed {} is owned by uid {actual}, not daemon uid {expected}; repair ownership before running `pohunek integration install --agent {}`",
                kind.description(),
                agent.kind().as_wire()
            ));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return false;
        }
    }

    let mode = metadata.permissions().mode() & UNIX_MODE_MASK;
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
        require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
    } else {
        warnings.push(format!(
            "managed {description} permissions drifted (mode {mode:04o}; expected {MANAGED_HOOK_MODE:04o}); run `pohunek integration install --agent {}` to restore them",
            agent.as_wire()
        ));
        require_recovery(recovery, IntegrationRecovery::Reinstall);
    }
    false
}

#[cfg(not(unix))]
fn asset_metadata_current(
    _agent: StatusAgent,
    _kind: ManagedAssetKind,
    _metadata: &fs::Metadata,
    _warnings: &mut Vec<String>,
    _recovery: &mut IntegrationRecovery,
) -> bool {
    true
}

fn installed_version(assets: &[AssetStatus], warnings: &mut Vec<String>) -> Option<u32> {
    let mut versions = Vec::new();
    for content in assets.iter().filter_map(|asset| asset.content.as_deref()) {
        versions.push(parse_integration_version(content)?);
    }
    versions.sort_unstable();
    versions.dedup();
    if versions.len() > 1 {
        warnings.push("managed asset version markers are inconsistent".to_owned());
        None
    } else {
        versions.first().copied()
    }
}

enum BoundedRead {
    Content(String),
    Oversized,
}

fn read_bounded_utf8_from_file(file: &mut fs::File, max_bytes: usize) -> io::Result<BoundedRead> {
    let read_limit = max_bytes.saturating_add(INSPECTION_LIMIT_PROBE_BYTES);
    let read_limit = u64::try_from(read_limit).map_err(|_error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "inspection limit does not fit in u64",
        )
    })?;
    let mut bytes = Vec::new();
    file.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Ok(BoundedRead::Oversized);
    }
    String::from_utf8(bytes)
        .map(BoundedRead::Content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn open_inspection_file(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
    };
    options.open(path)
}

#[cfg(unix)]
fn open_inspection_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options.open(path)
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

fn noncurrent_hook_file(footprint: bool, spec_count: usize) -> HookFileStatus {
    HookFileStatus {
        footprint,
        current: false,
        positions: vec![None; spec_count],
    }
}

enum ReadState {
    Missing,
    Unreadable,
    Oversized,
    Content(String),
}

fn inspect_claude_registration(
    config_dir: &Path,
    state_path: &Path,
    notify_path: &Path,
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
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
            label: "Claude SubagentStart".to_owned(),
            event: SUBAGENT_START_EVENT,
            command: hook_command(state_path, SUBAGENT_START_ACTION),
            matcher: Some("*"),
            trust_event: None,
        },
        HookSpec {
            label: "Claude SubagentStop".to_owned(),
            event: SUBAGENT_STOP_EVENT,
            command: hook_command(state_path, SUBAGENT_STOP_ACTION),
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
        recovery,
    );
    (status.footprint, status.current)
}

fn inspect_codex_registration(
    config_dir: &Path,
    state_path: &Path,
    notify_path: &Path,
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
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
            label: "Codex SubagentStart".to_owned(),
            event: SUBAGENT_START_EVENT,
            command: hook_command(state_path, SUBAGENT_START_ACTION),
            matcher: None,
            trust_event: Some(CODEX_SUBAGENT_START_TRUST_EVENT),
        },
        HookSpec {
            label: "Codex SubagentStop".to_owned(),
            event: SUBAGENT_STOP_EVENT,
            command: hook_command(state_path, SUBAGENT_STOP_ACTION),
            matcher: None,
            trust_event: Some(CODEX_SUBAGENT_STOP_TRUST_EVENT),
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
        recovery,
    );
    let config = inspect_codex_config(
        &config_dir.join("config.toml"),
        &hooks_path,
        &specs,
        &hooks.positions,
        warnings,
        recovery,
    );
    (hooks.footprint || config.0, hooks.current && config.1)
}

fn inspect_hook_file(
    agent: StatusAgent,
    path: &Path,
    label: &str,
    specs: &[HookSpec],
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
) -> HookFileStatus {
    let content = match read_status_file(path, label, warnings, recovery) {
        ReadState::Missing => return noncurrent_hook_file(false, specs.len()),
        ReadState::Unreadable | ReadState::Oversized => {
            return noncurrent_hook_file(true, specs.len());
        }
        ReadState::Content(content) => content,
    };
    let document: Value = match serde_json::from_str(&content) {
        Ok(document) => document,
        Err(_error) => {
            warnings.push(format!("{label} is malformed"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return noncurrent_hook_file(true, specs.len());
        }
    };
    let Some(root) = document.as_object() else {
        warnings.push(format!("{label} top level is not an object"));
        require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
        return noncurrent_hook_file(true, specs.len());
    };
    let hooks = match root.get("hooks") {
        Some(Value::Object(hooks)) => hooks,
        None => {
            warnings.push(format!("{label} has no hooks object"));
            require_recovery(recovery, IntegrationRecovery::Reinstall);
            return noncurrent_hook_file(false, specs.len());
        }
        Some(_invalid) => {
            warnings.push(format!("{label} has an invalid hooks structure"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return noncurrent_hook_file(true, specs.len());
        }
    };
    if let Some(event) = specs
        .iter()
        .map(|spec| spec.event)
        .find(|event| hooks.get(*event).is_some_and(|entries| !entries.is_array()))
    {
        warnings.push(format!("{label} has invalid {event} hook entries"));
        require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
        return noncurrent_hook_file(true, specs.len());
    }

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
            require_recovery(recovery, IntegrationRecovery::Reinstall);
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
    let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    let handler_matches = handlers.get(handler_index) == Some(&expected);
    // Codex trusts the complete normalized group, so siblings change identity.
    let canonical_codex_group = spec.trust_event.is_none() || handlers.as_slice() == [expected];
    matcher_matches && handler_matches && canonical_codex_group
}

fn inspect_codex_config(
    path: &Path,
    hooks_path: &Path,
    specs: &[HookSpec],
    positions: &[Option<(usize, usize)>],
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
) -> (bool, bool) {
    let content = match read_status_file(path, "Codex config.toml", warnings, recovery) {
        ReadState::Missing => return (false, false),
        ReadState::Unreadable | ReadState::Oversized => return (true, false),
        ReadState::Content(content) => content,
    };
    let document: DocumentMut = match content.parse() {
        Ok(document) => document,
        Err(_error) => {
            warnings.push("Codex config.toml is malformed".to_owned());
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return (true, false);
        }
    };
    let trust_prefix = format!("{}:", hooks_path.display());
    if !codex_config_structure_valid(&document, &trust_prefix) {
        warnings.push(
            "Codex config.toml has invalid features, hooks, or hooks.state structure".to_owned(),
        );
        require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
        return (true, false);
    }
    let expected_keys = specs
        .iter()
        .zip(positions)
        .filter_map(|(spec, position)| {
            let trust_event = spec.trust_event?;
            let (group_index, handler_index) = (*position)?;
            Some(codex_hook_trust_key(
                hooks_path,
                trust_event,
                group_index,
                handler_index,
            ))
        })
        .collect::<BTreeSet<_>>();
    let hooks_enabled = document
        .as_table()
        .get("features")
        .and_then(Item::as_table)
        .and_then(|features| features.get("hooks"))
        .and_then(Item::as_bool)
        == Some(true);
    if !hooks_enabled {
        warnings.push("Codex hooks feature is not enabled".to_owned());
        require_recovery(recovery, IntegrationRecovery::Reinstall);
    }

    let trust_state = document
        .as_table()
        .get("hooks")
        .and_then(Item::as_table)
        .and_then(|hooks| hooks.get("state"))
        .and_then(Item::as_table);
    let owned_hashes = spec_trust_hashes(specs);
    let actual_keys = trust_state
        .into_iter()
        .flat_map(|state| state.iter())
        .filter(|(key, item)| is_owned_trust_record(key, item, &trust_prefix, &owned_hashes))
        .map(|(key, _item)| key.to_owned())
        .collect::<BTreeSet<_>>();
    let footprint = !actual_keys.is_empty();
    let mut trust_current = true;
    for (spec, position) in specs.iter().zip(positions) {
        let Some(trust_event) = spec.trust_event else {
            continue;
        };
        let valid = position.is_some_and(|(group_index, handler_index)| {
            let trust_key =
                codex_hook_trust_key(hooks_path, trust_event, group_index, handler_index);
            let expected = codex_command_hook_trusted_hash(
                trust_event,
                &spec.command,
                HOOK_TIMEOUT_SECS,
                None,
            )
            .ok();
            trust_state
                .and_then(|state| state.get(&trust_key))
                .and_then(Item::as_table)
                .and_then(|entry| entry.get("trusted_hash"))
                .and_then(Item::as_str)
                .zip(expected.as_deref())
                .is_some_and(|(actual, expected)| actual == expected)
        });
        if !valid {
            warnings.push(format!(
                "managed {} trust record is missing or modified",
                spec.label
            ));
            require_recovery(recovery, IntegrationRecovery::Reinstall);
        }
        trust_current &= valid;
    }
    if actual_keys != expected_keys {
        warnings.push("managed Codex trust state contains stale or unexpected keys".to_owned());
        require_recovery(recovery, IntegrationRecovery::Reinstall);
        trust_current = false;
    }
    (footprint, hooks_enabled && trust_current)
}

/// The trusted hashes of the managed hooks described by `specs`.
fn spec_trust_hashes(specs: &[HookSpec]) -> OwnedTrustHashes {
    specs
        .iter()
        .filter_map(|spec| {
            let trust_event = spec.trust_event?;
            codex_command_hook_trusted_hash(trust_event, &spec.command, HOOK_TIMEOUT_SECS, None)
                .ok()
                .map(|hash| (trust_event, hash))
        })
        .collect()
}

fn codex_config_structure_valid(document: &DocumentMut, trust_prefix: &str) -> bool {
    let root = document.as_table();
    if root.get("features").is_some_and(|item| !item.is_table()) {
        return false;
    }
    let Some(hooks) = root.get("hooks") else {
        return true;
    };
    let Some(hooks) = hooks.as_table() else {
        return false;
    };
    let Some(state) = hooks.get("state") else {
        return true;
    };
    let Some(state) = state.as_table() else {
        return false;
    };
    state
        .iter()
        .all(|(key, item)| !is_managed_trust_key(key, trust_prefix) || item.is_table())
}

fn is_managed_trust_key(key: &str, trust_prefix: &str) -> bool {
    key.strip_prefix(trust_prefix).is_some_and(|suffix| {
        CODEX_MANAGED_TRUST_EVENTS.iter().any(|event| {
            suffix
                .strip_prefix(event)
                .is_some_and(|rest| rest.starts_with(':'))
        })
    })
}

fn read_status_file(
    path: &Path,
    label: &str,
    warnings: &mut Vec<String>,
    recovery: &mut IntegrationRecovery,
) -> ReadState {
    let mut file = match open_inspection_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warnings.push(format!("{label} is missing"));
            require_recovery(recovery, IntegrationRecovery::Reinstall);
            return ReadState::Missing;
        }
        Err(_error) => {
            warnings.push(format!("{label} could not be opened safely"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return ReadState::Unreadable;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_error) => {
            warnings.push(format!("{label} metadata could not be read"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            return ReadState::Unreadable;
        }
    };
    if !registration_metadata_current(label, &metadata, warnings) {
        require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
        return ReadState::Unreadable;
    }
    match read_bounded_utf8_from_file(&mut file, PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES) {
        Ok(BoundedRead::Content(content)) => ReadState::Content(content),
        Ok(BoundedRead::Oversized) => {
            warnings.push(format!(
                "{label} exceeds the {PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES}-byte inspection limit; reduce or replace the file before rerunning status"
            ));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            ReadState::Oversized
        }
        Err(_error) => {
            warnings.push(format!("{label} could not be read"));
            require_recovery(recovery, IntegrationRecovery::RepairConfiguration);
            ReadState::Unreadable
        }
    }
}

#[cfg(unix)]
fn registration_metadata_current(
    label: &str,
    metadata: &fs::Metadata,
    warnings: &mut Vec<String>,
) -> bool {
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    match evaluate_owner_private_metadata(
        metadata_snapshot(metadata, metadata.file_type().is_file()),
        effective_uid,
    ) {
        Ok(()) => true,
        Err(MetadataTrustIssue::UnexpectedType) => {
            warnings.push(format!("{label} is not a regular file"));
            false
        }
        Err(MetadataTrustIssue::ForeignOwner { actual, expected }) => {
            warnings.push(format!(
                "{label} is owned by uid {actual}, not daemon uid {expected}"
            ));
            false
        }
        Err(MetadataTrustIssue::GroupOrOtherWritable { mode }) => {
            warnings.push(format!(
                "{label} permissions are unsafe (mode {mode:04o}; group or other users can modify provider registration)"
            ));
            false
        }
    }
}

#[cfg(not(unix))]
fn registration_metadata_current(
    label: &str,
    metadata: &fs::Metadata,
    warnings: &mut Vec<String>,
) -> bool {
    if metadata.file_type().is_file() {
        true
    } else {
        warnings.push(format!("{label} is not a regular file"));
        false
    }
}

fn report(agent: RuntimeRef, paths: &InstallPaths) -> IntegrationInstallReport {
    IntegrationInstallReport {
        agent,
        hook_path: paths.hook_path.display().to_string(),
        config_paths: paths
            .config_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        cleanup_incomplete: paths.cleanup_incomplete.clone(),
        home: None,
    }
}

/// Claude's config dir as the built-in descriptor and this test process's
/// environment name it.
#[cfg(test)]
pub fn claude_config_dir() -> Result<PathBuf, ProtocolError> {
    homes::process_environment::runtime_config_dir(&RuntimeRef::claude())
}

/// Codex's config dir as the built-in descriptor and this test process's
/// environment name it.
#[cfg(test)]
pub fn codex_config_dir() -> Result<PathBuf, ProtocolError> {
    homes::process_environment::runtime_config_dir(&RuntimeRef::codex())
}

/// Install the Claude hooks into `claude_dir`.
///
/// Writes managed scripts under `hooks/` and merges `SessionStart`,
/// `SessionEnd`, `SubagentStart`, `SubagentStop`, `Notification`, `Stop`, and
/// `StopFailure` hooks into `settings.json`,
/// stripping any hooks this installer owns first so reinstall is idempotent.
///
/// # Errors
///
/// Fails fast if `claude_dir` is absent, if `settings.json` is malformed, or on
/// any I/O error.
pub fn install_claude(claude_dir: &Path) -> Result<InstallPaths, ProtocolError> {
    install_claude_gated(claude_dir, &mut |_index, _name| Ok(()))
}

/// [`install_claude`] with a gate run before each committed file.
fn install_claude_gated(
    claude_dir: &Path,
    gate: StepGate<'_>,
) -> Result<InstallPaths, ProtocolError> {
    stage_claude(claude_dir)?.commit_install(gate)
}

/// A Claude asset set computed against the current config directory and held
/// under the installer lock.
///
/// Nothing in the config directory has changed: the hooks directory is created
/// and every file written only by [`StagedClaude::commit_install`].
#[derive(Debug)]
struct StagedClaude {
    config_root: TrustedDir,
    hooks_dir: PathBuf,
    trusted_hooks: Option<TrustedDir>,
    hook_path: PathBuf,
    settings_path: PathBuf,
    settings_file: Option<LoadedFile>,
    settings_mode: u32,
    settings_body: String,
    manifest: AssetManifest,
    _install_lock: InstallerLock,
}

/// Computes the Claude asset set for `claude_dir` without changing it.
fn stage_claude(claude_dir: &Path) -> Result<StagedClaude, ProtocolError> {
    validate_config_dir(claude_dir.to_path_buf(), "Claude config directory")?;
    if config_path_kind(claude_dir) == ConfigPath::Symlink {
        return Err(config_dir_is_symlink(claude_dir));
    }
    if !claude_dir.is_dir() {
        return Err(config_dir_missing(StatusAgent::Claude, claude_dir));
    }

    let config_root = TrustedDir::open(claude_dir, "Claude config directory")?;
    let install_lock = config_root.lock_installer()?;
    let hooks_dir = claude_dir.join("hooks");
    let trusted_hooks = config_root.open_child("hooks", "Claude hooks directory")?;
    let hook_path = hooks_dir.join(STATE_HOOK_INSTALL_NAME);
    let notify_hook_path = hooks_dir.join(NOTIFY_HOOK_INSTALL_NAME);

    let settings_path = claude_dir.join("settings.json");
    let settings_file = config_root.read_optional("settings.json", "Claude settings.json")?;
    let settings_mode = settings_file
        .as_ref()
        .map_or(NEW_REGISTRATION_MODE, |file| file.mode);
    let mut settings = parse_json_object_or_empty(
        settings_file.as_ref().map(|file| file.content.as_str()),
        &settings_path,
    )?;
    merge_claude_hooks(&mut settings, &settings_path, &hook_path, &notify_hook_path)?;
    let settings_body = json_pretty(&settings_path, &settings)?;
    let manifest = AssetManifest {
        version: EXPECTED_INTEGRATION_VERSION,
        active_version: active_version(
            trusted_hooks.as_ref(),
            STATE_HOOK_INSTALL_NAME,
            "Claude state hook",
        ),
        actions: registered_actions(
            &settings,
            &[
                ManagedScript {
                    path: &hook_path,
                    asset: CLAUDE_HOOK_ASSET,
                },
                ManagedScript {
                    path: &notify_hook_path,
                    asset: CLAUDE_NOTIFY_HOOK_ASSET,
                },
            ],
        )?,
    };

    Ok(StagedClaude {
        config_root,
        hooks_dir,
        trusted_hooks,
        hook_path,
        settings_path,
        settings_file,
        settings_mode,
        settings_body,
        manifest,
        _install_lock: install_lock,
    })
}

impl StagedClaude {
    /// Activates the staged set: scripts first, registration last, as one
    /// rollback-capable transaction.
    fn commit_install(self, gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError> {
        let Self {
            config_root,
            hooks_dir,
            trusted_hooks,
            hook_path,
            settings_path,
            settings_file,
            settings_mode,
            settings_body,
            _install_lock,
            ..
        } = self;
        let created_hooks = trusted_hooks.is_none();
        let trusted_hooks = match trusted_hooks {
            Some(hooks) => hooks,
            None => config_root.create_child(
                "hooks",
                "Claude hooks directory",
                MANAGED_HOOK_DIR_MODE,
            )?,
        };
        let steps = [
            Step {
                dir: &trusted_hooks,
                name: STATE_HOOK_INSTALL_NAME,
                label: "Claude state hook",
                action: Action::Write {
                    body: CLAUDE_HOOK_ASSET,
                    mode: MANAGED_HOOK_MODE,
                },
                expected: Expected::Any,
            },
            Step {
                dir: &trusted_hooks,
                name: NOTIFY_HOOK_INSTALL_NAME,
                label: "Claude notification hook",
                action: Action::Write {
                    body: CLAUDE_NOTIFY_HOOK_ASSET,
                    mode: MANAGED_HOOK_MODE,
                },
                expected: Expected::Any,
            },
            Step {
                dir: &config_root,
                name: "settings.json",
                label: "Claude settings.json",
                action: Action::Write {
                    body: &settings_body,
                    mode: settings_mode,
                },
                expected: Expected::Loaded(settings_file.as_ref()),
            },
        ];
        let committed = commit::commit(&steps, gate, commit::Operation::Install);
        if let Err(error) = &committed {
            // A directory this install created is removed again once the rollback
            // left it empty; after an incomplete rollback it stays for inspection.
            if created_hooks && error.code != commit::RECOVERY_REQUIRED_CODE {
                if let Err(quarantine) =
                    config_root.remove_created_empty_child("hooks", &trusted_hooks)
                {
                    return Err(commit::recovery_required(
                        &[format!(
                            "{} (the created hooks directory and any files in it stay at {})",
                            hooks_dir.display(),
                            quarantine.display()
                        )],
                        Some(&error.code),
                        commit::Operation::Install,
                    ));
                }
            }
        }
        let committed = committed?;

        Ok(InstallPaths {
            hook_path,
            config_paths: vec![settings_path],
            cleanup_incomplete: committed.cleanup_incomplete,
        })
    }
}

/// Version marker of the active managed state hook in `dir`, when one is
/// readable and carries a valid marker.
///
/// A script that cannot be read safely has no known version; the commit step
/// that replaces it decides what happens to it.
fn active_version(dir: Option<&TrustedDir>, name: &str, label: &str) -> Option<u32> {
    let file = dir?.read_optional(name, label).ok().flatten()?;
    parse_integration_version(&file.content)
}

/// Merges the managed Claude hook registrations into `settings`.
///
/// Only commands this installer wrote are stripped before the managed set is
/// added, so reinstalling never duplicates them and never touches user hooks.
fn merge_claude_hooks(
    settings: &mut Value,
    settings_path: &Path,
    hook_path: &Path,
    notify_hook_path: &Path,
) -> Result<(), ProtocolError> {
    let hooks = ensure_hooks_object(settings, settings_path)?;
    let state_hook_commands = [
        hook_command(hook_path, HOOK_ACTION),
        hook_command(hook_path, HOOK_RELEASE_ACTION),
        hook_command(hook_path, SUBAGENT_START_ACTION),
        hook_command(hook_path, SUBAGENT_STOP_ACTION),
    ];
    remove_owned_command_hooks(hooks, &state_hook_commands);
    remove_owned_command_hooks(hooks, &claude_notify_hook_commands(notify_hook_path));
    ensure_command_hook(
        hooks,
        SESSION_START_EVENT,
        &state_hook_commands[0],
        Some("*"),
    )?;
    ensure_command_hook(hooks, SESSION_END_EVENT, &state_hook_commands[1], Some("*"))?;
    ensure_command_hook(
        hooks,
        SUBAGENT_START_EVENT,
        &state_hook_commands[2],
        Some("*"),
    )?;
    ensure_command_hook(
        hooks,
        SUBAGENT_STOP_EVENT,
        &state_hook_commands[3],
        Some("*"),
    )?;
    for matcher in CLAUDE_NOTIFICATION_MATCHERS {
        ensure_command_hook(
            hooks,
            CLAUDE_NOTIFICATION_EVENT,
            &hook_command_with_args(notify_hook_path, &[NOTIFICATION_ACTION, matcher]),
            Some(matcher),
        )?;
    }
    ensure_command_hook(
        hooks,
        CLAUDE_STOP_EVENT,
        &hook_command(notify_hook_path, STOP_ACTION),
        Some("*"),
    )?;
    ensure_command_hook(
        hooks,
        CLAUDE_STOP_FAILURE_EVENT,
        &hook_command(notify_hook_path, STOP_FAILURE_ACTION),
        Some("*"),
    )?;
    Ok(())
}

/// Install the Codex hooks into `codex_dir`.
///
/// Writes managed scripts, merges `SessionStart`, `SubagentStart`,
/// `SubagentStop`, `PermissionRequest`, and `Stop` hooks into `hooks.json`, and
/// enables `[features] hooks = true` in `config.toml`, idempotently.
///
/// # Errors
///
/// Fails fast if `codex_dir` is absent, if `hooks.json` is malformed, or on any
/// I/O error.
pub fn install_codex(codex_dir: &Path) -> Result<InstallPaths, ProtocolError> {
    install_codex_gated(codex_dir, &mut |_index, _name| Ok(()))
}

/// [`install_codex`] with a gate run before each committed file.
fn install_codex_gated(
    codex_dir: &Path,
    gate: StepGate<'_>,
) -> Result<InstallPaths, ProtocolError> {
    stage_codex(codex_dir)?.commit_install(gate)
}

/// A Codex asset set computed against the current config directory and held
/// under the installer lock.
///
/// Nothing in the config directory has changed: every file is written only by
/// [`StagedCodex::commit_install`].
#[derive(Debug)]
struct StagedCodex {
    config_root: TrustedDir,
    hook_path: PathBuf,
    hooks_path: PathBuf,
    hooks_source: Option<LoadedFile>,
    hooks_mode: u32,
    hooks_body: String,
    config_path: PathBuf,
    config_source: Option<LoadedFile>,
    config_mode: u32,
    config_body: String,
    manifest: AssetManifest,
    _install_lock: InstallerLock,
}

/// Computes the Codex asset set for `codex_dir` without changing it.
#[expect(
    clippy::too_many_lines,
    reason = "Codex hook and trust registration is one atomic configuration transaction"
)]
fn stage_codex(codex_dir: &Path) -> Result<StagedCodex, ProtocolError> {
    validate_config_dir(codex_dir.to_path_buf(), "Codex config directory")?;
    if config_path_kind(codex_dir) == ConfigPath::Symlink {
        return Err(config_dir_is_symlink(codex_dir));
    }
    if !codex_dir.is_dir() {
        return Err(config_dir_missing(StatusAgent::Codex, codex_dir));
    }

    let config_root = TrustedDir::open(codex_dir, "Codex config directory")?;
    let install_lock = config_root.lock_installer()?;
    let hook_path = codex_dir.join(STATE_HOOK_INSTALL_NAME);
    let notify_hook_path = codex_dir.join(NOTIFY_HOOK_INSTALL_NAME);

    let hooks_path = codex_dir.join("hooks.json");
    let hooks_source = config_root.read_optional("hooks.json", "Codex hooks.json")?;
    let hooks_mode = hooks_source
        .as_ref()
        .map_or(NEW_REGISTRATION_MODE, |file| file.mode);
    let mut hooks_file = parse_json_object_or_empty(
        hooks_source.as_ref().map(|file| file.content.as_str()),
        &hooks_path,
    )?;
    let hooks = ensure_hooks_object(&mut hooks_file, &hooks_path)?;
    let hooks_before = hooks.clone();
    remove_owned_command_hooks(
        hooks,
        &[
            hook_command(&hook_path, HOOK_ACTION),
            hook_command(&hook_path, SUBAGENT_START_ACTION),
            hook_command(&hook_path, SUBAGENT_STOP_ACTION),
        ],
    );
    remove_owned_command_hooks(hooks, &codex_notify_hook_commands(&notify_hook_path));
    // Codex exposes `Stop` for turn completion, not session/process exit. Do
    // not wire state release to it; procwatch is the lifecycle backstop.
    let codex_hooks = codex_managed_hooks(&hook_path, &notify_hook_path);
    for managed in &codex_hooks {
        ensure_command_hook(hooks, managed.event, &managed.command, None)?;
    }
    let trust_moves = trust_rekeys(&hooks_path, &hooks_before, hooks, &codex_hooks)?;
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
    let config_path = codex_dir.join("config.toml");
    let config_source = config_root.read_optional("config.toml", "Codex config.toml")?;
    let config_mode = config_source
        .as_ref()
        .map_or(NEW_REGISTRATION_MODE, |file| file.mode);
    let existing = config_source
        .as_ref()
        .map_or("", |file| file.content.as_str());
    let mut trust_states = Vec::with_capacity(trust_entries.len());
    for (trust_event, command, group_index, handler_index) in trust_entries {
        let trust_key = codex_hook_trust_key(&hooks_path, trust_event, group_index, handler_index);
        let trusted_hash =
            codex_command_hook_trusted_hash(trust_event, &command, HOOK_TIMEOUT_SECS, None)?;
        trust_states.push((trust_key, trusted_hash));
    }
    let owned_hashes = owned_trust_hashes(&codex_hooks)?;
    let updated = update_codex_config_toml(
        existing,
        &trust_states,
        &config_path,
        &hooks_path,
        &owned_hashes,
        &trust_moves,
    )?;
    let hooks_body = json_pretty(&hooks_path, &hooks_file)?;
    let manifest = AssetManifest {
        version: EXPECTED_INTEGRATION_VERSION,
        active_version: active_version(
            Some(&config_root),
            STATE_HOOK_INSTALL_NAME,
            "Codex state hook",
        ),
        actions: registered_actions(
            &hooks_file,
            &[
                ManagedScript {
                    path: &hook_path,
                    asset: CODEX_HOOK_ASSET.as_str(),
                },
                ManagedScript {
                    path: &notify_hook_path,
                    asset: CODEX_NOTIFY_HOOK_ASSET.as_str(),
                },
            ],
        )?,
    };

    Ok(StagedCodex {
        config_root,
        hook_path,
        hooks_path,
        hooks_source,
        hooks_mode,
        hooks_body,
        config_path,
        config_source,
        config_mode,
        config_body: updated,
        manifest,
        _install_lock: install_lock,
    })
}

impl StagedCodex {
    /// Activates the staged set: scripts first, registration last, as one
    /// rollback-capable transaction.
    fn commit_install(self, gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError> {
        let Self {
            config_root,
            hook_path,
            hooks_path,
            hooks_source,
            hooks_mode,
            hooks_body,
            config_path,
            config_source,
            config_mode,
            config_body: updated,
            _install_lock,
            ..
        } = self;
        let existing = config_source
            .as_ref()
            .map_or("", |file| file.content.as_str());
        // An unchanged config.toml is still an input of this install's decisions, so
        // it is verified before any destructive step and again right before the
        // registration step; a changed one is written with its exact expectation.
        let config_unchanged = updated == existing;
        let verify_config = || Step {
            dir: &config_root,
            name: "config.toml",
            label: "Codex config.toml",
            action: Action::Verify,
            expected: Expected::Loaded(config_source.as_ref()),
        };
        let mut steps = Vec::new();
        if config_unchanged {
            steps.push(verify_config());
        }
        steps.push(Step {
            dir: &config_root,
            name: STATE_HOOK_INSTALL_NAME,
            label: "Codex state hook",
            action: Action::Write {
                body: CODEX_HOOK_ASSET.as_str(),
                mode: MANAGED_HOOK_MODE,
            },
            expected: Expected::Any,
        });
        steps.push(Step {
            dir: &config_root,
            name: NOTIFY_HOOK_INSTALL_NAME,
            label: "Codex notification hook",
            action: Action::Write {
                body: CODEX_NOTIFY_HOOK_ASSET.as_str(),
                mode: MANAGED_HOOK_MODE,
            },
            expected: Expected::Any,
        });
        if config_unchanged {
            steps.push(verify_config());
        }
        steps.push(Step {
            dir: &config_root,
            name: "hooks.json",
            label: "Codex hooks.json",
            action: Action::Write {
                body: &hooks_body,
                mode: hooks_mode,
            },
            expected: Expected::Loaded(hooks_source.as_ref()),
        });
        if !config_unchanged {
            steps.push(Step {
                dir: &config_root,
                name: "config.toml",
                label: "Codex config.toml",
                action: Action::Write {
                    body: &updated,
                    mode: config_mode,
                },
                expected: Expected::Loaded(config_source.as_ref()),
            });
        }
        let committed = commit::commit(&steps, gate, commit::Operation::Install)?;

        Ok(InstallPaths {
            hook_path,
            config_paths: vec![hooks_path, config_path],
            cleanup_incomplete: committed.cleanup_incomplete,
        })
    }
}

struct CodexManagedHook {
    event: &'static str,
    trust_event: &'static str,
    command: String,
}

/// The five hooks the Codex installer registers, in registration order.
fn codex_managed_hooks(hook_path: &Path, notify_hook_path: &Path) -> [CodexManagedHook; 5] {
    [
        CodexManagedHook {
            event: SESSION_START_EVENT,
            trust_event: CODEX_SESSION_START_TRUST_EVENT,
            command: hook_command(hook_path, HOOK_ACTION),
        },
        CodexManagedHook {
            event: SUBAGENT_START_EVENT,
            trust_event: CODEX_SUBAGENT_START_TRUST_EVENT,
            command: hook_command(hook_path, SUBAGENT_START_ACTION),
        },
        CodexManagedHook {
            event: SUBAGENT_STOP_EVENT,
            trust_event: CODEX_SUBAGENT_STOP_TRUST_EVENT,
            command: hook_command(hook_path, SUBAGENT_STOP_ACTION),
        },
        CodexManagedHook {
            event: CODEX_PERMISSION_REQUEST_EVENT,
            trust_event: CODEX_PERMISSION_REQUEST_TRUST_EVENT,
            command: hook_command(notify_hook_path, PERMISSION_REQUEST_ACTION),
        },
        CodexManagedHook {
            event: CODEX_STOP_EVENT,
            trust_event: CODEX_STOP_TRUST_EVENT,
            command: hook_command(notify_hook_path, STOP_ACTION),
        },
    ]
}

/// The `(trust event, trusted hash)` pairs of the managed hooks.
///
/// A trust record is installer-owned only when its hash is one of these: the
/// key alone is derived from a position, and a user's own hook in the same
/// event can occupy any position.
type OwnedTrustHashes = BTreeSet<(&'static str, String)>;

fn owned_trust_hashes(hooks: &[CodexManagedHook]) -> Result<OwnedTrustHashes, ProtocolError> {
    hooks
        .iter()
        .map(|hook| {
            codex_command_hook_trusted_hash(
                hook.trust_event,
                &hook.command,
                HOOK_TIMEOUT_SECS,
                None,
            )
            .map(|hash| (hook.trust_event, hash))
        })
        .collect()
}

/// Whether a `hooks.state` entry is a trust record of a managed hook.
///
/// It must sit in the managed key namespace of `trust_prefix` and carry the
/// exact trusted hash of one of the managed commands for that event, at any
/// position. Records for a user's own hooks never match.
fn is_owned_trust_record(
    key: &str,
    item: &Item,
    trust_prefix: &str,
    owned: &OwnedTrustHashes,
) -> bool {
    let Some(suffix) = key.strip_prefix(trust_prefix) else {
        return false;
    };
    let Some(event) = CODEX_MANAGED_TRUST_EVENTS.iter().find(|event| {
        suffix
            .strip_prefix(**event)
            .is_some_and(|rest| rest.starts_with(':'))
    }) else {
        return false;
    };
    item.as_table()
        .and_then(|table| table.get("trusted_hash"))
        .and_then(Item::as_str)
        .is_some_and(|hash| owned.contains(&(*event, hash.to_owned())))
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

/// The hook operation a managed script reports for its action argument
/// `argument`.
fn script_action(argument: &str) -> Option<HookAction> {
    match argument {
        HOOK_ACTION => Some(HookAction::IdentityReport),
        HOOK_RELEASE_ACTION => Some(HookAction::IdentityRelease),
        SUBAGENT_START_ACTION => Some(HookAction::SubagentStart),
        SUBAGENT_STOP_ACTION => Some(HookAction::SubagentStop),
        PERMISSION_REQUEST_ACTION | NOTIFICATION_ACTION | STOP_ACTION | STOP_FAILURE_ACTION => {
            Some(HookAction::Notification)
        }
        _ => None,
    }
}

/// The action arguments a managed script accepts: the patterns of the first
/// `case "$action" in` block other than the catch-all.
///
/// Returns `None` when the script has no such block or the block lists no
/// argument, so a layout the parser does not know never reads as an empty
/// table.
fn script_accepted_arguments(script: &str) -> Option<Vec<&str>> {
    let mut lines = script.lines();
    lines.find(|line| line.trim() == r#"case "$action" in"#)?;
    let mut arguments = Vec::new();
    for line in lines {
        let line = line.trim();
        if line == "esac" {
            break;
        }
        let Some((patterns, _body)) = line.split_once(')') else {
            continue;
        };
        arguments.extend(
            patterns
                .split('|')
                .map(str::trim)
                .filter(|pattern| *pattern != "*"),
        );
    }
    (!arguments.is_empty()).then_some(arguments)
}

/// A managed script together with the path it is installed at.
struct ManagedScript<'a> {
    /// Installed path the registered command runs.
    path: &'a Path,
    /// The embedded script that is written to `path`.
    asset: &'a str,
}

/// The hook operations a registration document makes the managed scripts
/// report: every command hook that runs a managed script and passes an
/// argument that script accepts, mapped through [`script_action`].
///
/// A hook of the user that runs a managed script counts like a hook the
/// installer wrote, because the script reports for either. An argument the
/// script ignores reports nothing.
///
/// # Errors
///
/// Fails closed when an embedded script's argument table cannot be read or
/// accepts an argument that maps to no operation.
fn registered_actions(
    registration: &Value,
    scripts: &[ManagedScript<'_>],
) -> Result<Vec<HookAction>, ProtocolError> {
    let mut tables = Vec::with_capacity(scripts.len());
    for script in scripts {
        let accepted = script_accepted_arguments(script.asset)
            .ok_or_else(|| unreadable_action_table(script.path))?;
        let prefix = hook_command_with_args(script.path, &[]) + " ";
        tables.push((prefix, accepted));
    }
    let commands = registration
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::values)
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|handler| handler.get("type").and_then(Value::as_str) == Some("command"))
        .filter_map(|handler| handler.get("command").and_then(Value::as_str));
    let mut actions = Vec::new();
    for command in commands {
        for (prefix, accepted) in &tables {
            let Some(argument) = command
                .strip_prefix(prefix.as_str())
                .and_then(|rest| rest.split_whitespace().next())
                .filter(|argument| accepted.contains(argument))
            else {
                continue;
            };
            let action = script_action(argument)
                .ok_or_else(|| unmapped_script_argument(prefix, argument))?;
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
    }
    Ok(actions)
}

/// The error of an embedded script whose argument table cannot be read.
fn unreadable_action_table(path: &Path) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_asset_unreadable",
        format!(
            "the managed script {} has no readable action table",
            path.display()
        ),
        None,
    )
}

/// The error of an embedded script that accepts an argument no hook
/// operation is defined for.
fn unmapped_script_argument(prefix: &str, argument: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_asset_unreadable",
        format!(
            "the managed script `{}` accepts the action {argument}, which maps to no hook operation",
            prefix.trim_end()
        ),
        None,
    )
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

/// Whether a hook entry carries one of this installer's exact command identities.
fn is_owned_command(hook: &Value, owned_commands: &[String]) -> bool {
    hook.get("command")
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
    hooks_path: &Path,
    owned: &OwnedTrustHashes,
    moves: &[(String, String)],
) -> Result<String, ProtocolError> {
    let mut doc = content.parse::<DocumentMut>().map_err(|err| {
        settings_invalid(
            path,
            &format!(
                "invalid TOML in Codex config.toml: {}",
                toml_error_summary(content, &err)
            ),
        )
    })?;

    ensure_table(doc.as_table_mut(), "features", path)?.insert("hooks", value(true));
    let hooks = ensure_table(doc.as_table_mut(), "hooks", path)?;
    let state = ensure_table(hooks, "state", path)?;
    let trust_prefix = format!("{}:", hooks_path.display());
    if state
        .iter()
        .any(|(key, item)| is_managed_trust_key(key, &trust_prefix) && !item.is_table())
    {
        return Err(settings_invalid(
            path,
            "cannot replace a scalar managed Codex trust entry",
        ));
    }
    let stale_keys = state
        .iter()
        .filter(|(key, item)| is_owned_trust_record(key, item, &trust_prefix, owned))
        .map(|(key, _item)| key.to_owned())
        .collect::<Vec<_>>();
    for stale_key in stale_keys {
        state.remove(&stale_key);
    }
    apply_trust_moves(state, moves, path)?;
    for (trust_key, trusted_hash) in trust_states {
        ensure_table(state, trust_key, path)?.insert("trusted_hash", value(trusted_hash.as_str()));
    }

    Ok(doc.to_string())
}

/// Error code for trust records that cannot be re-keyed without choosing between
/// two claims on one key.
const TRUST_CONFLICT_CODE: &str = "integration_trust_conflict";

fn trust_conflict(path: &Path, detail: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Configuration,
        TRUST_CONFLICT_CODE,
        format!(
            "{}: cannot keep the trust records of your own hooks: {detail}",
            path.display()
        ),
        Some(
            "review the [hooks.state] tables in config.toml by hand so each hook has at most one record at its current position, then repeat the command"
                .to_owned(),
        ),
    )
}

/// Positions `(group, handler)` of the user's own handlers in `event`: every
/// handler whose command is not one of `owned`, in file order.
fn user_handler_positions(
    hooks: &Map<String, Value>,
    event: &str,
    owned: &[String],
) -> Vec<(usize, usize)> {
    let Some(groups) = hooks.get(event).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut positions = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        let handlers = group
            .get("hooks")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for (handler_index, handler) in handlers.iter().enumerate() {
            if !is_owned_command(handler, owned) {
                positions.push((group_index, handler_index));
            }
        }
    }
    positions
}

/// The trust-record moves a change of `hooks.json` from `before` to `after`
/// requires.
///
/// A trust key embeds the group and handler index, so removing or adding a
/// managed group shifts the keys of the user's own hooks in the same event.
/// Survivors keep their relative order, so the k-th user handler before is the
/// k-th after; the returned `(old key, new key)` pairs move each record with
/// its hook. The trusted hash covers only the handler and its matcher, so it
/// stays valid at the new position.
fn trust_rekeys(
    hooks_path: &Path,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    managed: &[CodexManagedHook],
) -> Result<Vec<(String, String)>, ProtocolError> {
    let owned: Vec<String> = managed.iter().map(|hook| hook.command.clone()).collect();
    let mut moves = Vec::new();
    for hook in managed {
        let old = user_handler_positions(before, hook.event, &owned);
        let new = user_handler_positions(after, hook.event, &owned);
        if old.len() != new.len() {
            return Err(trust_conflict(
                hooks_path,
                "the user's hooks in one event changed while their trust records were being kept",
            ));
        }
        for (from, to) in old.into_iter().zip(new) {
            if from != to {
                moves.push((
                    codex_hook_trust_key(hooks_path, hook.trust_event, from.0, from.1),
                    codex_hook_trust_key(hooks_path, hook.trust_event, to.0, to.1),
                ));
            }
        }
    }
    Ok(moves)
}

/// Moves trust records to their hooks' new keys.
///
/// Every source is taken out first, so a move into a slot another move vacates
/// is fine; a destination still occupied by any other record is a conflict and
/// nothing is chosen.
fn apply_trust_moves(
    state: &mut Table,
    moves: &[(String, String)],
    path: &Path,
) -> Result<(), ProtocolError> {
    let taken: Vec<(String, Item)> = moves
        .iter()
        .filter_map(|(from, to)| state.remove(from).map(|item| (to.clone(), item)))
        .collect();
    for (to, item) in taken {
        if state.contains_key(&to) {
            return Err(trust_conflict(
                path,
                &format!("another trust record already claims the key {to}"),
            ));
        }
        state.insert(&to, item);
    }
    Ok(())
}

/// Describes a TOML parse failure by position and message only.
///
/// The parser's `Display` output quotes the offending source line, which may
/// hold a credential, so it never reaches an error response.
fn toml_error_summary(content: &str, error: &toml_edit::TomlError) -> String {
    let Some(span) = error.span() else {
        return error.message().to_owned();
    };
    let consumed = &content.as_bytes()[..span.start.min(content.len())];
    let line = consumed.split(|byte| *byte == b'\n').count();
    let line_start = consumed
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let column = consumed.len() - line_start + 1;
    format!("{} (line {line}, column {column})", error.message())
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

fn validate_config_dir(path: PathBuf, source: &str) -> Result<PathBuf, ProtocolError> {
    if !path.is_absolute() || path.to_str().is_none() {
        return Err(config_dir_invalid(source));
    }
    Ok(path)
}

/// `agent_config_dir_invalid`: the value of `source` (an environment variable
/// or `HOME`) is not an absolute UTF-8 path. The value itself is never named.
fn config_dir_invalid(source: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Configuration,
        "agent_config_dir_invalid",
        format!("{source} must resolve to an absolute UTF-8 path for agent hook registration"),
        Some("set the provider config directory to an absolute path".to_owned()),
    )
}

#[cfg(unix)]
#[derive(Debug)]
struct TrustedDir {
    path: PathBuf,
    file: fs::File,
    trusted: PlatformTrustedDir,
}

#[cfg(unix)]
#[derive(Debug)]
struct LoadedFile {
    content: String,
    mode: u32,
    /// The inode `content` was read from.
    identity: pohunek_platform::filesystem::EntryIdentity,
}

#[cfg(unix)]
impl TrustedDir {
    fn open(path: &Path, label: &str) -> Result<Self, ProtocolError> {
        let trusted = PlatformTrustedDir::open_absolute_owner_safe(path, GROUP_OR_OTHER_WRITE_MASK)
            .map_err(|error| platform_path_error(path, label, &error))?;
        let file = trusted
            .try_clone_descriptor()
            .map_err(|error| platform_path_error(path, label, &error))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            trusted,
        })
    }

    fn open_child(&self, name: &str, label: &str) -> Result<Option<Self>, ProtocolError> {
        let path = self.path.join(name);
        let trusted = match self
            .trusted
            .open_child_owner_safe(name, GROUP_OR_OTHER_WRITE_MASK)
        {
            Ok(trusted) => trusted,
            Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => return Ok(None),
            Err(error) => return Err(platform_path_error(&path, label, &error)),
        };
        let file = trusted
            .try_clone_descriptor()
            .map_err(|error| platform_path_error(&path, label, &error))?;
        Ok(Some(Self {
            path,
            file,
            trusted,
        }))
    }

    fn create_child(&self, name: &str, label: &str, mode: u32) -> Result<Self, ProtocolError> {
        let path = self.path.join(name);
        let trusted = self
            .trusted
            .open_or_create_child(name, mode)
            .map_err(|error| platform_path_error(&path, label, &error))?;
        let file = trusted
            .try_clone_descriptor()
            .map_err(|error| platform_path_error(&path, label, &error))?;
        Ok(Self {
            path,
            file,
            trusted,
        })
    }

    fn read_optional(&self, name: &str, label: &str) -> Result<Option<LoadedFile>, ProtocolError> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let path = self.path.join(name);
        let fd = match rustix::fs::openat(
            &self.file,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(path_open_untrusted(&path, label, error)),
        };
        let file = fs::File::from(fd);
        let metadata = file
            .metadata()
            .map_err(|error| io_error("read trusted file metadata", &path, &error))?;
        validate_trusted_metadata(&path, label, &metadata, false)?;
        let mode = metadata.permissions().mode() & UNIX_MODE_MASK;
        let bytes = self
            .trusted
            .read_file(name, mode, PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES)
            .map_err(|error| platform_path_error(&path, label, &error))?;
        let content = String::from_utf8(bytes).map_err(|error| {
            io_error(
                "decode trusted file as UTF-8",
                &path,
                &io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?;
        let identity = self
            .trusted
            .entry_identity(name, EntryKind::RegularFile)
            .map_err(|error| platform_path_error(&path, label, &error))?
            .filter(|identity| {
                identity.device() == metadata.dev() && identity.inode() == metadata.ino()
            })
            .ok_or_else(|| path_untrusted(&path, &format!("{label} changed while it was read")))?;
        Ok(Some(LoadedFile {
            content,
            mode,
            identity,
        }))
    }

    /// Removes the directory `name` that the caller created, if it is still
    /// that same directory and empty.
    ///
    /// A directory that cannot be proven identical is left in place untouched,
    /// because an empty directory is harmless and a wrong removal is not. Once
    /// the directory has been moved aside it must not stay hidden: any outcome
    /// that leaves it (or files that appeared in it) in quarantine, whether the
    /// removal was refused, a restore collided, or a step failed after the
    /// rename, is the path where it now lives.
    fn remove_created_empty_child(&self, name: &str, created: &TrustedDir) -> Result<(), PathBuf> {
        use std::os::unix::fs::MetadataExt as _;

        use quarantine::{Settled, Staging};

        let Ok(Some(identity)) = self.trusted.entry_identity(name, EntryKind::Directory) else {
            // Nothing was moved: the directory is not there or not provably ours.
            return Ok(());
        };
        let Ok(metadata) = created.file.metadata() else {
            return Ok(());
        };
        if metadata.dev() != identity.device() || metadata.ino() != identity.inode() {
            return Ok(());
        }
        let staged =
            match quarantine::stage(&self.trusted, name, CREATED_DIR_QUARANTINE_PREFIX, identity) {
                Ok(Staging::Moved(staged)) => staged,
                // Nothing was moved, so the directory is exactly where it was.
                Ok(Staging::Missing | Staging::Refused) => return Ok(()),
                Err(error) => return created_dir_stage_failure(&error),
            };
        commit::race_point("created_dir.staged", name);
        let empty = created
            .trusted
            .entry_names()
            .is_ok_and(|entries| entries.is_empty());
        commit::race_point("created_dir.checked", name);
        let settled = if empty {
            quarantine::discard(staged)
        } else {
            quarantine::put_back(&staged, name)
        };
        match settled {
            Settled::Done | Settled::Unconfirmed(_) => Ok(()),
            Settled::Left { at, .. } => Err(at),
        }
    }

    /// Takes the exclusive installer lock for this config directory.
    ///
    /// The lock file is a regular file inside the directory, so every
    /// installer that targets the same agent home or profile contends on it
    /// and installers for different profiles never do.
    fn lock_installer(&self) -> Result<InstallerLock, ProtocolError> {
        let path = self.path.join(INSTALL_LOCK_NAME);
        self.trusted
            .lock_file(INSTALL_LOCK_NAME, INSTALL_LOCK_MODE, LockKind::Exclusive)
            .map(|lock| InstallerLock { _lock: lock })
            .map_err(|error| {
                match error {
                FsError::LockContended { .. } => ProtocolError::new(
                    ErrorClass::Runtime,
                    INSTALL_IN_PROGRESS_CODE,
                    format!(
                        "another integration install, uninstall or doctor holds {}",
                        path.display()
                    ),
                    Some(
                        "retry once the other integration install, uninstall or doctor has finished"
                            .to_owned(),
                    ),
                ),
                other => platform_path_error(&path, "integration install lock", &other),
            }
            })
    }
}

/// The result of a failed attempt to move the created directory aside.
///
/// A failure that names where the directory stays left it in quarantine and
/// must be reported; one that does not left it exactly where it was.
fn created_dir_stage_failure(error: &FsError) -> Result<(), PathBuf> {
    error
        .recovery_path()
        .map_or(Ok(()), |at| Err(at.to_path_buf()))
}

/// Exclusive installer lock; released when dropped.
#[cfg(unix)]
#[derive(Debug)]
struct InstallerLock {
    _lock: pohunek_platform::filesystem::FileLock,
}

#[cfg(unix)]
fn platform_path_error(
    path: &Path,
    label: &str,
    error: &pohunek_platform::filesystem::FsError,
) -> ProtocolError {
    path_untrusted(
        path,
        &format!("{label} failed trusted filesystem validation: {error}"),
    )
}

#[cfg(unix)]
fn validate_trusted_metadata(
    path: &Path,
    label: &str,
    metadata: &fs::Metadata,
    expected_directory: bool,
) -> Result<(), ProtocolError> {
    let expected_type = if expected_directory {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_file()
    };
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    match evaluate_owner_private_metadata(metadata_snapshot(metadata, expected_type), effective_uid)
    {
        Ok(()) => Ok(()),
        Err(MetadataTrustIssue::UnexpectedType) => Err(path_untrusted(
            path,
            &format!("{label} has an unexpected filesystem type"),
        )),
        Err(MetadataTrustIssue::ForeignOwner { actual, expected }) => Err(path_untrusted(
            path,
            &format!("{label} is owned by uid {actual}, not daemon uid {expected}"),
        )),
        Err(MetadataTrustIssue::GroupOrOtherWritable { mode }) => Err(path_untrusted(
            path,
            &format!("{label} is group/world writable (mode {mode:04o})"),
        )),
    }
}

#[cfg(unix)]
fn path_open_untrusted(path: &Path, label: &str, error: rustix::io::Errno) -> ProtocolError {
    path_untrusted(
        path,
        &format!(
            "{label} could not be opened safely: {}",
            io::Error::from(error)
        ),
    )
}

fn path_untrusted(path: &Path, detail: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Configuration,
        "integration_path_untrusted",
        format!("untrusted integration path {}: {detail}", path.display()),
        Some("repair path ownership, type, and permissions, then repeat the command".to_owned()),
    )
}

fn parse_json_object_or_empty(content: Option<&str>, path: &Path) -> Result<Value, ProtocolError> {
    content.map_or_else(
        || Ok(json!({})),
        |content| {
            serde_json::from_str::<Value>(content)
                .map_err(|error| settings_invalid(path, &format!("invalid JSON: {error}")))
        },
    )
}

fn json_pretty(path: &Path, value: &Value) -> Result<String, ProtocolError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| settings_invalid(path, &format!("could not serialize settings: {error}")))
}

fn config_dir_missing(agent: StatusAgent, dir: &Path) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_config_dir_missing",
        format!(
            "{} config dir not found at {}",
            agent.kind().as_wire(),
            dir.display()
        ),
        Some(agent.missing_config_hint().to_owned()),
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

fn committed_durability_error(
    path: &Path,
    source: &pohunek_platform::filesystem::FsError,
) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_write_committed_durability_uncertain",
        format!(
            "integration file replacement committed for {}, but durability is uncertain: {source}",
            path.display()
        ),
        Some("inspect the installed integration before retrying".to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use pohunek_test_support::process_env::ProcessEnv;
    use protocol::method;
    use protocol::{RuntimeId, RuntimeRef};
    use serde_json::{json, Value};

    use super::{
        codex_command_hook_trusted_hash, codex_hook_trust_key, hook_command, install_claude,
        install_codex, shell_single_quote, toml_basic_string, CLAUDE_HOOK_ASSET, CODEX_HOOK_ASSET,
        CODEX_NOTIFY_HOOK_ASSET, CODEX_SESSION_START_TRUST_EVENT,
        CODEX_SUBAGENT_COMPATIBILITY_FIXTURE, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID,
        ENV_SOCKET_PATH, EXPECTED_INTEGRATION_VERSION, HOOK_TIMEOUT_SECS,
        INTEGRATION_VERSION_PREFIX, NOTIFY_HOOK_INSTALL_NAME, SESSION_START_EVENT,
        STATE_HOOK_INSTALL_NAME, SUBAGENT_START_ACTION, SUBAGENT_START_EVENT, SUBAGENT_STOP_ACTION,
        SUBAGENT_STOP_EVENT,
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
    /// Integration asset version expected after bounded in-memory state hooks ship.
    const STATE_ASSET_VERSION_HEADER: &str = "# POHUNEK_INTEGRATION_VERSION=11";
    /// Writable inheritable ACL used to prove mode bits alone are insufficient on macOS.
    #[cfg(target_os = "macos")]
    const WRITABLE_INHERITABLE_ACL: &str = "everyone allow read,write,execute,delete,append,readattr,writeattr,readextattr,writeextattr,readsecurity,file_inherit,directory_inherit";
    /// Action argument for state-hook `SessionStart` reporting.
    const STATE_SESSION_ACTION: &str = "session";
    /// Action argument for state-hook release reporting.
    const STATE_RELEASE_ACTION: &str = "release";
    /// Claude lifecycle event used for active-agent release.
    const CLAUDE_SESSION_END_EVENT: &str = "SessionEnd";
    /// Marks the isolated child process that may safely change process umask.
    #[cfg(unix)]
    const CLAUDE_HOOKS_UMASK_CHILD_ENV: &str = "POHUNEK_TEST_CLAUDE_HOOKS_UMASK_CHILD";

    pub(super) use crate::test_support::ScopedDir as TestDir;

    /// Creates a per-test directory that is removed when the guard drops.
    pub(super) fn scoped_dir(tag: &str) -> TestDir {
        crate::test_support::scoped_dir(&format!("ph-int-{tag}-"))
    }

    pub(super) fn read_json(path: &Path) -> Value {
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
        super::runnable_script(agent, script)
    }

    fn notification_asset(agent: &str) -> PathBuf {
        daemon_manifest_asset(agent, "pohunek-agent-notify.sh")
    }

    pub(super) fn state_asset(agent: &str) -> PathBuf {
        daemon_manifest_asset(agent, "pohunek-agent-state.sh")
    }

    fn run_state_asset(
        agent: &str,
        args: &[&str],
        input: &Value,
        socket_available: bool,
        expected_requests: usize,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>) {
        run_state_asset_in(
            agent,
            args,
            input,
            socket_available,
            expected_requests,
            None,
        )
    }

    /// Runs the state hook with `cwd` as its working directory (the test's own
    /// when `None`).
    fn run_state_asset_in(
        agent: &str,
        args: &[&str],
        input: &Value,
        socket_available: bool,
        expected_requests: usize,
        cwd: Option<&Path>,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>) {
        let temp = scoped_dir(&format!("{agent}-state-run"));
        let socket_path = temp.join("daemon.sock");
        run_state_asset_at(
            agent,
            args,
            input,
            socket_available.then_some(socket_path.as_path()),
            &socket_path,
            expected_requests,
            cwd,
        )
    }

    /// Runs the state hook with `POHUNEK_SOCKET_PATH` set to `env_socket_path`.
    ///
    /// When `listener_path` is `Some`, a real listener bound there captures
    /// `expected_requests` requests; it may differ from `env_socket_path` when
    /// the hook reaches the socket through an alias.
    pub(super) fn run_state_asset_at(
        agent: &str,
        args: &[&str],
        input: &Value,
        listener_path: Option<&Path>,
        env_socket_path: &Path,
        expected_requests: usize,
        cwd: Option<&Path>,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>) {
        let asset_path = state_asset(agent);
        assert!(
            asset_path.is_file(),
            "missing state hook asset at {}",
            asset_path.display()
        );

        // Read before the capture thread starts so waiting for the lock cannot
        // eat into its deadline.
        let path = inherited_path();
        let temp = scoped_dir(&format!("{agent}-state-run"));
        let socket_path = env_socket_path;
        let handle = listener_path.map(|listener_path| {
            let listener = UnixListener::bind(listener_path).expect("bind hook socket");
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
                            // BSD-derived accept inherits the listener's nonblocking flag.
                            stream
                                .set_nonblocking(false)
                                .expect("make daemon hook stream blocking");
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

        let mut command = Command::new("/bin/sh");
        command
            .arg(&asset_path)
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("TMPDIR", &*temp)
            .env(ENV_FLAG, "1")
            .env(ENV_SOCKET_PATH, socket_path)
            .env(ENV_SESSION_ID, "session-123")
            .env(
                ENV_PROTOCOL_VERSION,
                protocol::PROTOCOL_VERSION.get().to_string(),
            )
            .env("POHUNEK_WORKER_INSTANCE_ID", "runtime-123")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
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

    fn run_state_asset_with_large_input(
        agent: &str,
        action: &str,
        input: &[u8],
    ) -> (
        std::process::ExitStatus,
        String,
        String,
        usize,
        Vec<PathBuf>,
    ) {
        let temp = scoped_dir(&format!("{agent}-state-bounded-input"));
        let mut child = Command::new("/bin/sh")
            .arg(state_asset(agent))
            .arg(action)
            .env_clear()
            .env("PATH", inherited_path())
            .env("TMPDIR", &*temp)
            .env(ENV_FLAG, "1")
            .env(
                "POHUNEK_WORKER_SOCKET_PATH",
                temp.join("missing-worker.sock"),
            )
            .env(ENV_SESSION_ID, "session-123")
            .env("POHUNEK_WORKER_INSTANCE_ID", "runtime-123")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bounded state hook");
        let mut bytes_written = 0;
        if let Some(mut stdin) = child.stdin.take() {
            while bytes_written < input.len() {
                match stdin.write(&input[bytes_written..]) {
                    Ok(0) => break,
                    Ok(written) => bytes_written += written,
                    Err(err) if err.kind() == ErrorKind::BrokenPipe => break,
                    Err(err) => panic!("write state hook stdin: {err}"),
                }
            }
        }
        let output = child
            .wait_with_output()
            .expect("wait for bounded state hook");
        let files = fs::read_dir(&temp)
            .expect("read state hook temp dir")
            .map(|entry| entry.expect("read state hook temp entry").path())
            .collect::<Vec<_>>();
        (
            output.status,
            String::from_utf8(output.stdout).expect("state hook stdout utf8"),
            String::from_utf8(output.stderr).expect("state hook stderr utf8"),
            bytes_written,
            files,
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

    /// Accepts one worker hook request on `worker_socket`, answers it with
    /// `worker_response`, and returns the parsed request.
    pub(super) fn capture_worker_hook(
        worker_socket: &Path,
        worker_response: &'static [u8],
    ) -> thread::JoinHandle<Value> {
        let listener = UnixListener::bind(worker_socket).expect("bind worker hook socket");
        listener
            .set_nonblocking(true)
            .expect("make worker hook socket nonblocking");
        thread::spawn(move || {
            // Bounded so a hook that never reaches the worker fails the test
            // instead of blocking it forever.
            let deadline =
                std::time::Instant::now() + Duration::from_secs(HOOK_CAPTURE_TIMEOUT_SECS);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _addr)) => break stream,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "worker hook never connected to the worker socket"
                        );
                        thread::sleep(Duration::from_millis(HOOK_CAPTURE_POLL_MS));
                    }
                    Err(err) => panic!("accept worker hook: {err}"),
                }
            };
            // BSD-derived accept inherits the listener's nonblocking flag.
            stream
                .set_nonblocking(false)
                .expect("make worker hook stream blocking");
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
        })
    }

    fn run_worker_state_asset_with_response(
        agent: &str,
        action: &str,
        input: &Value,
        worker_response: &'static [u8],
        expected_public_requests: usize,
    ) -> (Value, Vec<Value>) {
        run_worker_state_asset_with_env(
            agent,
            action,
            input,
            worker_response,
            expected_public_requests,
            &[("POHUNEK_WORKER_INSTANCE_ID", "runtime-123")],
        )
    }

    /// Runs the state hook with `instance_env` as the worker instance
    /// environment variables the worker injected.
    fn run_worker_state_asset_with_env(
        agent: &str,
        action: &str,
        input: &Value,
        worker_response: &'static [u8],
        expected_public_requests: usize,
        instance_env: &[(&str, &str)],
    ) -> (Value, Vec<Value>) {
        let asset_path = state_asset(agent);
        let path = inherited_path();
        let temp = scoped_dir(&format!("{agent}-worker-state-run"));
        let worker_socket = temp.join("worker.sock");
        let daemon_socket = temp.join("daemon.sock");
        let capture = capture_worker_hook(&worker_socket, worker_response);
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
                        // BSD-derived accept inherits the listener's nonblocking flag.
                        stream
                            .set_nonblocking(false)
                            .expect("make daemon hook stream blocking");
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

        let mut child = Command::new("/bin/sh")
            .arg(asset_path)
            .arg(action)
            .env_clear()
            .env("PATH", path)
            .env("TMPDIR", &*temp)
            .env(ENV_FLAG, "1")
            .env("POHUNEK_WORKER_SOCKET_PATH", &worker_socket)
            .env("POHUNEK_NATIVE_REFERENCE_KIND", "id")
            .env(ENV_SOCKET_PATH, &daemon_socket)
            .env(ENV_SESSION_ID, "session-123")
            .env(
                ENV_PROTOCOL_VERSION,
                protocol::PROTOCOL_VERSION.get().to_string(),
            )
            .envs(instance_env.iter().copied())
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
            NotificationRunOptions::default(),
        );
        (status, stdout, stderr, requests)
    }

    /// Optional inputs of `run_notification_asset_custom`.
    #[derive(Clone, Copy)]
    struct NotificationRunOptions<'a> {
        /// Value of the session-id environment variable; unset when `None`.
        session_id: Option<&'a str>,
        /// `TMPDIR` for the hook instead of the run's own temp directory.
        tmpdir_override: Option<&'a Path>,
        /// Tolerate the hook closing stdin before all input was written.
        allow_broken_pipe: bool,
        /// Working directory of the hook process.
        cwd: Option<&'a Path>,
    }

    impl Default for NotificationRunOptions<'_> {
        fn default() -> Self {
            Self {
                session_id: Some("session-123"),
                tmpdir_override: None,
                allow_broken_pipe: false,
                cwd: None,
            }
        }
    }

    /// Protocol version a session launched by this build carries in its env.
    fn current_protocol_version() -> String {
        protocol::PROTOCOL_VERSION.get().to_string()
    }

    fn run_notification_asset_custom(
        agent: &str,
        args: &[&str],
        input: &[u8],
        socket_available: bool,
        options: NotificationRunOptions<'_>,
    ) -> (std::process::ExitStatus, String, String, Vec<Value>, usize) {
        let NotificationRunOptions {
            session_id,
            tmpdir_override,
            allow_broken_pipe,
            cwd,
        } = options;
        let asset_path = notification_asset(agent);
        assert!(
            asset_path.is_file(),
            "missing notification hook asset at {}",
            asset_path.display()
        );

        let path = inherited_path();
        let temp = scoped_dir(&format!("{agent}-notify-run"));
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
                            // BSD-derived accept inherits the listener's nonblocking flag.
                            stream
                                .set_nonblocking(false)
                                .expect("make daemon hook stream blocking");
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

        let mut command = Command::new("/bin/sh");
        command
            .arg(&asset_path)
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("TMPDIR", tmpdir)
            .env(ENV_FLAG, "1")
            .env(ENV_SOCKET_PATH, &socket_path)
            .env(ENV_PROTOCOL_VERSION, current_protocol_version())
            .env("POHUNEK_SECRET_SENTINEL", "DROP_ME_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session_id) = session_id {
            command.env(ENV_SESSION_ID, session_id);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
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
                NotificationRunOptions {
                    session_id: Some("session-123\nhostile"),
                    ..NotificationRunOptions::default()
                },
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
                NotificationRunOptions {
                    allow_broken_pipe: true,
                    ..NotificationRunOptions::default()
                },
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
                NotificationRunOptions {
                    allow_broken_pipe: true,
                    ..NotificationRunOptions::default()
                },
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
            let temp = scoped_dir(&format!("{agent}-broken-tmpdir"));
            let missing_tmpdir = temp.join("missing");
            let (status, stdout, stderr, requests, _bytes_written) = run_notification_asset_custom(
                agent,
                &args,
                br#"{"hook_event_id":"broken-tmpdir"}"#,
                false,
                NotificationRunOptions {
                    tmpdir_override: Some(&missing_tmpdir),
                    allow_broken_pipe: true,
                    ..NotificationRunOptions::default()
                },
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
            // hermetic-allowed: #363 the command text is hashed into a golden Codex trust vector
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
        for (agent, asset) in [
            ("claude", CLAUDE_HOOK_ASSET),
            ("codex", CODEX_HOOK_ASSET.as_str()),
        ] {
            assert!(
                asset.starts_with("#!/bin/sh"),
                "hook must be a POSIX sh script"
            );
            assert!(
                asset.contains(STATE_ASSET_VERSION_HEADER),
                "{agent} hook must carry the current integration version"
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
                asset.contains("python3 -I - 3<&0 <<'PY' || exit 0"),
                "the python heredoc must be guarded with `|| exit 0`"
            );
            assert!(
                !asset.contains("mktemp") && !asset.contains("POHUNEK_HOOK_INPUT_FILE"),
                "{agent} state hook must not persist provider payloads"
            );
            assert!(
                asset.contains("handle.read(MAX_HOOK_INPUT_BYTES + 1)"),
                "{agent} state hook must bound provider input before decoding"
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

    /// A worker that sets only `POHUNEK_WORKER_INSTANCE_ID` and a worker that
    /// sets only `POHUNEK_RUNTIME_ID` both identify the same instance to the
    /// state hooks.
    #[test]
    fn state_hooks_accept_either_worker_instance_environment_name() {
        let input = json!({"session_id": "native-1", "transcript_path": "/work/t.jsonl"});
        for agent in ["claude", "codex"] {
            for (name, instance_env) in [
                ("current", [("POHUNEK_WORKER_INSTANCE_ID", "runtime-123")]),
                ("runtime id only", [("POHUNEK_RUNTIME_ID", "runtime-123")]),
            ] {
                let (report, public_requests) = run_worker_state_asset_with_env(
                    agent,
                    STATE_SESSION_ACTION,
                    &input,
                    WORKER_HOOK_SUCCESS_RESPONSE,
                    0,
                    &instance_env,
                );

                assert_eq!(report["type"], "identity_report", "{agent} {name}");
                assert_eq!(report["runtime_id"], "runtime-123", "{agent} {name}");
                assert!(public_requests.is_empty(), "{agent} {name}");
            }
        }
    }

    /// With both names set, `POHUNEK_WORKER_INSTANCE_ID` wins.
    #[test]
    fn state_hooks_prefer_the_worker_instance_environment_name() {
        let input = json!({"session_id": "native-1", "transcript_path": "/work/t.jsonl"});
        for agent in ["claude", "codex"] {
            let (report, _public) = run_worker_state_asset_with_env(
                agent,
                STATE_SESSION_ACTION,
                &input,
                WORKER_HOOK_SUCCESS_RESPONSE,
                0,
                &[
                    ("POHUNEK_RUNTIME_ID", "runtime-other"),
                    ("POHUNEK_WORKER_INSTANCE_ID", "runtime-123"),
                ],
            );

            assert_eq!(report["runtime_id"], "runtime-123", "{agent}");
        }
    }

    /// The hook runs in the agent's working directory, which must neither slow
    /// the interpreter's imports nor shadow the standard library.
    #[test]
    fn state_hooks_ignore_modules_in_the_working_directory() {
        for agent in ["claude", "codex"] {
            let workdir = scoped_dir(&format!("{agent}-shadowed-cwd"));
            let marker = workdir.join("shadow-imported");
            for module in ["json", "os", "socket", "time", "datetime"] {
                fs::write(
                    workdir.join(format!("{module}.py")),
                    format!("open({marker:?}, 'w').close()\nraise ImportError('shadowed')\n"),
                )
                .expect("write shadow module");
            }
            let input = json!({
                "session_id": format!("{agent}-native"),
                "transcript_path": format!("/work/{agent}-transcript.jsonl"),
            });

            let (status, _stdout, stderr, requests) = run_state_asset_in(
                agent,
                &[STATE_SESSION_ACTION],
                &input,
                true,
                STATE_SESSION_REQUEST_COUNT,
                Some(&workdir),
            );

            assert!(
                status.success(),
                "{agent} state hook exited with {status}: {stderr}"
            );
            assert!(
                !marker.exists(),
                "{agent} state hook imported a module from its working directory"
            );
            assert_eq!(
                requests.len(),
                STATE_SESSION_REQUEST_COUNT,
                "{agent} state hook must still report from a shadowed working directory"
            );
        }
    }

    /// The notification hooks run in the agent's working directory as well.
    #[test]
    fn notification_hooks_ignore_modules_in_the_working_directory() {
        for agent in ["claude", "codex"] {
            let workdir = scoped_dir(&format!("{agent}-notify-shadowed-cwd"));
            let marker = workdir.join("shadow-imported");
            for module in ["json", "os", "socket", "time", "datetime"] {
                fs::write(
                    workdir.join(format!("{module}.py")),
                    format!("open({marker:?}, 'w').close()\nraise ImportError('shadowed')\n"),
                )
                .expect("write shadow module");
            }

            let (status, _stdout, stderr, requests, _bytes_written) = run_notification_asset_custom(
                agent,
                &["stop"],
                br#"{"hook_event_id":"shadowed-cwd"}"#,
                true,
                NotificationRunOptions {
                    cwd: Some(&workdir),
                    ..NotificationRunOptions::default()
                },
            );

            assert!(
                status.success(),
                "{agent} notification hook exited with {status}: {stderr}"
            );
            assert!(
                !marker.exists(),
                "{agent} notification hook imported a module from its working directory"
            );
            assert_eq!(
                requests.len(),
                1,
                "{agent} notification hook must still report from a shadowed working directory"
            );
        }
    }

    #[test]
    fn state_hooks_send_pid_on_session_start_reports() {
        for agent in ["claude", "codex"] {
            let input = json!({
                "session_id": format!("{agent}-native"),
                "transcript_path": format!("/work/{agent}-transcript.jsonl"),
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
                json!(format!("/work/{agent}-transcript.jsonl"))
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
            assert_eq!(native["params"]["worker_instance_id"], json!("runtime-123"));
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
                json!(format!("/work/{agent}-transcript.jsonl"))
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
                    "transcript_path": format!("/work/{agent}.jsonl"),
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
    fn state_hooks_forward_only_sanitized_subagent_lifecycle_fields() {
        for agent in ["claude", "codex"] {
            let start = run_worker_state_asset(
                agent,
                SUBAGENT_START_ACTION,
                &json!({
                    "agent_id": "child-1",
                    "agent_type": "Explore",
                    "parent_agent_id": "parent-1",
                    "prompt": "must not cross the hook boundary",
                    "transcript_path": "/secret/transcript.jsonl",
                }),
            );
            assert_eq!(start["type"], "subagent_start");
            assert_eq!(start["provider"], agent);
            assert_eq!(start["subagent_id"], "child-1");
            assert_eq!(start["agent_type"], "Explore");
            assert_eq!(start["parent_id"], "parent-1");
            assert!(start.get("prompt").is_none());
            assert!(start.get("transcript_path").is_none());

            let stop = run_worker_state_asset(
                agent,
                SUBAGENT_STOP_ACTION,
                &json!({
                    "agent_id": "child-1",
                    "last_assistant_message": "must not cross the hook boundary",
                }),
            );
            assert_eq!(stop["type"], "subagent_stop");
            assert_eq!(stop["subagent_id"], "child-1");
            assert_eq!(stop["outcome"], "completed");
            assert!(stop.get("last_assistant_message").is_none());
        }
    }

    #[test]
    fn state_hooks_validate_before_reading_and_bound_provider_input_without_files() {
        let oversized = vec![b'x'; LARGE_HOOK_INPUT_BYTES];
        for agent in ["claude", "codex"] {
            for action in ["ignored-action", SUBAGENT_START_ACTION] {
                let (status, stdout, stderr, bytes_written, files) =
                    run_state_asset_with_large_input(agent, action, &oversized);
                assert!(
                    status.success(),
                    "{agent} hook exited with {status}: {stderr}"
                );
                assert_eq!(stdout, "", "{agent} state hook must not print stdout");
                assert_eq!(stderr, "", "{agent} state hook must not print stderr");
                assert!(
                    bytes_written < LARGE_HOOK_INPUT_BYTES,
                    "{agent} {action} consumed the full oversized stdin"
                );
                assert!(
                    files.is_empty(),
                    "{agent} {action} persisted hook input: {files:?}"
                );
            }
        }
    }

    #[test]
    fn codex_subagent_compatibility_fixture_crosses_only_the_lifecycle_boundary() {
        let fixture: Value = serde_json::from_str(CODEX_SUBAGENT_COMPATIBILITY_FIXTURE)
            .expect("valid Codex compatibility fixture");

        let start = run_worker_state_asset("codex", SUBAGENT_START_ACTION, &fixture["start"]);
        assert_eq!(start["type"], "subagent_start");
        assert_eq!(start["provider"], "codex");
        assert_eq!(start["subagent_id"], "agent-child-1");
        assert_eq!(start["agent_type"], "reviewer");
        assert!(start.get("transcript_path").is_none());
        assert!(start.get("prompt").is_none());

        let stop = run_worker_state_asset("codex", SUBAGENT_STOP_ACTION, &fixture["stop"]);
        assert_eq!(stop["type"], "subagent_stop");
        assert_eq!(stop["provider"], "codex");
        assert_eq!(stop["subagent_id"], "agent-child-1");
        assert_eq!(stop["outcome"], "completed");
        assert!(stop.get("agent_transcript_path").is_none());
        assert!(stop.get("last_assistant_message").is_none());
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
    fn pending_worker_launch_claims_do_not_fall_back_to_public_methods() {
        const PENDING_RESPONSE: &[u8] = b"{\"ok\":true,\"launch_identity_accepted\":false,\"launch_identity_status\":\"pending\"}\n";
        for agent in ["claude", "codex"] {
            let (worker_request, public_requests) = run_worker_state_asset_with_response(
                agent,
                STATE_SESSION_ACTION,
                &json!({"session_id": format!("{agent}-native")}),
                PENDING_RESPONSE,
                0,
            );
            assert_eq!(worker_request["type"], "identity_report");
            assert!(public_requests.is_empty());
        }
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
            request["params"]["seq"]
                .as_str()
                .is_some_and(|seq| seq.parse::<u64>().is_ok()),
            "Claude release request must carry a fresh decimal-string sequence"
        );
    }

    #[test]
    fn install_claude_into_fresh_dir_writes_executable_hook_and_session_start() {
        let claude_dir = scoped_dir("claude-fresh");
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
        for (event, action) in [
            (SUBAGENT_START_EVENT, SUBAGENT_START_ACTION),
            (SUBAGENT_STOP_EVENT, SUBAGENT_STOP_ACTION),
        ] {
            let commands = command_hooks(&settings, event);
            assert_eq!(commands.len(), 1, "exactly one {event} hook");
            assert!(commands[0].ends_with(action));
        }
    }

    #[test]
    fn status_reports_missing_config_without_mutation() {
        let root = scoped_dir("status-missing");
        fs::create_dir_all(&root).expect("create temp root");
        let claude = root.join(".claude");
        let codex = root.join(".codex");

        let result = with_config_dirs(&claude, &codex, || {
            super::status(protocol::IntegrationStatusParams {
                agent: None,
                ..Default::default()
            })
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
            assert_eq!(report.recovery, protocol::IntegrationRecovery::None);
            assert!(
                report.warnings.is_empty(),
                "an agent that is not installed is informational: {:?}",
                report.warnings
            );
        }
        assert!(!claude.exists());
        assert!(!codex.exists());
    }

    #[test]
    fn bounded_status_read_accepts_limit_and_rejects_next_byte() {
        let root = scoped_dir("status-bounded-read");
        fs::create_dir_all(&root).expect("create bounded read root");
        let path = root.join("settings.json");
        fs::write(
            &path,
            vec![b' '; super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES],
        )
        .expect("write exact-limit file");

        let read = |path: &Path| {
            let mut file = fs::File::open(path).expect("open the inspected file");
            super::read_bounded_utf8_from_file(
                &mut file,
                super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES,
            )
        };

        match read(&path).expect("read exact-limit file") {
            super::BoundedRead::Content(content) => {
                assert_eq!(content.len(), super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES);
            }
            super::BoundedRead::Oversized => panic!("exact-limit file must be accepted"),
        }

        fs::write(
            &path,
            vec![b' '; super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES + 1],
        )
        .expect("write over-limit file");
        assert!(matches!(
            read(&path).expect("classify over-limit file"),
            super::BoundedRead::Oversized
        ));
    }

    #[test]
    fn status_reports_complete_installs_current_without_mutation() {
        let root = scoped_dir("status-current");
        let claude = root.join("claude");
        let codex = root.join("codex");
        fs::create_dir_all(&claude).expect("create Claude dir");
        fs::create_dir_all(&codex).expect("create Codex dir");
        install_claude(&claude).expect("install Claude fixture");
        install_codex(&codex).expect("install Codex fixture");
        let before = tree_snapshot(&root);

        let result = with_config_dirs(&claude, &codex, || {
            super::status(protocol::IntegrationStatusParams {
                agent: None,
                ..Default::default()
            })
        })
        .expect("status current");

        assert_eq!(tree_snapshot(&root), before, "status must not mutate files");
        assert_eq!(result.agents.len(), 2);
        for report in result.agents {
            assert_eq!(report.state, protocol::IntegrationInstallState::Current);
            assert_eq!(report.recovery, protocol::IntegrationRecovery::None);
            assert_eq!(report.installed_version, Some(EXPECTED_INTEGRATION_VERSION));
            assert_eq!(report.expected_asset_paths.len(), 2);
            assert_eq!(report.present_asset_paths.len(), 2);
            assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        }
    }

    #[cfg(unix)]
    #[test]
    fn registration_files_require_owner_safe_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        for (name, agent, relative_path) in [
            (
                "Claude settings.json",
                RuntimeRef::claude(),
                "settings.json",
            ),
            ("Codex hooks.json", RuntimeRef::codex(), "hooks.json"),
            ("Codex config.toml", RuntimeRef::codex(), "config.toml"),
        ] {
            let config_dir = scoped_dir(&format!("status-registration-mode-{name}"));
            match agent.as_wire() {
                RuntimeId::CLAUDE => {
                    install_claude(&config_dir).expect("install Claude fixture");
                }
                RuntimeId::CODEX => {
                    install_codex(&config_dir).expect("install Codex fixture");
                }
                _ => {
                    unreachable!("test uses managed agents")
                }
            }
            fs::set_permissions(
                config_dir.join(relative_path),
                fs::Permissions::from_mode(0o664),
            )
            .expect("make registration group writable");

            let report = explicit_status(&config_dir, agent);

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{name}"
            );
            assert_eq!(
                report.recovery,
                protocol::IntegrationRecovery::RepairConfiguration,
                "{name}"
            );
            assert!(
                report.warnings.iter().any(|warning| {
                    warning.contains(name)
                        && warning.contains("permissions are unsafe")
                        && warning.contains("mode 0664")
                }),
                "{name}: {:?}",
                report.warnings
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn registration_files_are_opened_without_following_symlinks() {
        use std::os::unix::fs::symlink;

        for (name, agent, relative_path) in [
            (
                "Claude settings.json",
                RuntimeRef::claude(),
                "settings.json",
            ),
            ("Codex hooks.json", RuntimeRef::codex(), "hooks.json"),
            ("Codex config.toml", RuntimeRef::codex(), "config.toml"),
        ] {
            let config_dir = scoped_dir(&format!("status-registration-symlink-{name}"));
            match agent.as_wire() {
                RuntimeId::CLAUDE => {
                    install_claude(&config_dir).expect("install Claude fixture");
                }
                RuntimeId::CODEX => {
                    install_codex(&config_dir).expect("install Codex fixture");
                }
                _ => {
                    unreachable!("test uses managed agents")
                }
            }
            let registration_path = config_dir.join(relative_path);
            let target_path = config_dir.join(format!("{relative_path}.target"));
            fs::rename(&registration_path, &target_path).expect("move registration target");
            symlink(&target_path, &registration_path).expect("link registration file");

            let report = explicit_status(&config_dir, agent);

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{name}"
            );
            assert_eq!(
                report.recovery,
                protocol::IntegrationRecovery::RepairConfiguration,
                "{name}"
            );
            assert!(
                report.warnings.iter().any(|warning| {
                    warning.contains(name) && warning.contains("could not be opened safely")
                }),
                "{name}: {:?}",
                report.warnings
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn installer_rejects_untrusted_parents_before_any_mutation() {
        use std::os::unix::fs::PermissionsExt as _;

        let claude = scoped_dir("install-untrusted-claude-hooks-parent");
        install_claude(&claude).expect("install trusted Claude fixture");
        fs::set_permissions(claude.join("hooks"), fs::Permissions::from_mode(0o775))
            .expect("make Claude hooks parent unsafe");
        let claude_before = tree_snapshot(&claude);

        let claude_error = install_claude(&claude).expect_err("reject unsafe Claude hooks parent");

        assert_eq!(claude_error.code, "integration_path_untrusted");
        assert_eq!(tree_snapshot(&claude), claude_before);

        let codex = scoped_dir("install-untrusted-codex-config-parent");
        install_codex(&codex).expect("install trusted Codex fixture");
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o775))
            .expect("make Codex config parent unsafe");
        let codex_before = tree_snapshot(&codex);

        let codex_error = install_codex(&codex).expect_err("reject unsafe Codex config parent");

        assert_eq!(codex_error.code, "integration_path_untrusted");
        assert_eq!(tree_snapshot(&codex), codex_before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installer_rejects_an_extended_acl_on_the_owner_safe_trust_root() {
        let codex = scoped_dir("install-untrusted-codex-acl-parent");
        let status = Command::new("/bin/chmod")
            .args(["+a", WRITABLE_INHERITABLE_ACL])
            .arg(&*codex)
            .status()
            .expect("run native ACL fixture command");
        assert!(status.success(), "native ACL fixture command must succeed");
        let before = tree_snapshot(&codex);

        let error = install_codex(&codex).expect_err("reject ACL-writable Codex config parent");

        assert_eq!(error.code, "integration_path_untrusted");
        assert_eq!(tree_snapshot(&codex), before);
    }

    #[cfg(unix)]
    #[test]
    fn trusted_dir_replace_is_anchored_against_parent_name_swap() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = scoped_dir("trusted-dir-name-swap");
        let trusted = super::TrustedDir::open(&root, "test root").expect("open trusted dirfd");
        let holder = scoped_dir("trusted-dir-name-moved");
        let moved = holder.join("opened");
        fs::rename(&root, &moved).expect("move opened directory");
        fs::create_dir(&root).expect("create replacement directory at original name");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("make replacement directory private");

        super::commit::commit(
            &[super::Step {
                dir: &trusted,
                name: "sentinel",
                label: "test file",
                action: super::Action::Write {
                    body: "trusted\n",
                    mode: 0o600,
                },
                expected: super::Expected::Any,
            }],
            &mut |_index, _name| Ok(()),
            super::commit::Operation::Install,
        )
        .expect("replace through opened dirfd");

        assert_eq!(
            fs::read_to_string(moved.join("sentinel")).expect("read dirfd target"),
            "trusted\n"
        );
        assert!(!root.join("sentinel").exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_claude_creates_private_hooks_under_owner_masking_umask() {
        use nix::sys::stat::{umask, Mode};
        use std::os::unix::fs::PermissionsExt as _;

        if std::env::var_os(CLAUDE_HOOKS_UMASK_CHILD_ENV).is_none() {
            let env =
                pohunek_test_support::env::TestEnv::new().expect("create the child environment");
            let output = env
                .command(std::env::current_exe().expect("current test executable"))
                .arg("--exact")
                .arg(
                    "integration::tests::install_claude_creates_private_hooks_under_owner_masking_umask",
                )
                .arg("--test-threads=1")
                .arg("--nocapture")
                .env(CLAUDE_HOOKS_UMASK_CHILD_ENV, "1")
                .output()
                .expect("spawn isolated umask test child");
            assert!(
                output.status.success(),
                "isolated umask child failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let new_config = scoped_dir("claude-hooks-new-under-umask");
        let existing_config = scoped_dir("claude-hooks-existing-under-umask");
        let existing_hooks = existing_config.join("hooks");
        fs::create_dir(&existing_hooks).expect("create existing hooks directory");
        fs::set_permissions(&existing_hooks, fs::Permissions::from_mode(0o750))
            .expect("set existing hooks mode");

        let previous_umask = umask(Mode::from_bits_truncate(0o700));
        install_claude(&new_config).expect("install Claude with new hooks directory");
        let new_mode = fs::symlink_metadata(new_config.join("hooks"))
            .expect("new hooks metadata")
            .permissions()
            .mode()
            & super::UNIX_MODE_MASK;
        let new_settings_mode = fs::symlink_metadata(new_config.join("settings.json"))
            .expect("new settings metadata")
            .permissions()
            .mode()
            & super::UNIX_MODE_MASK;
        let new_hook_mode =
            fs::symlink_metadata(new_config.join("hooks").join(STATE_HOOK_INSTALL_NAME))
                .expect("new managed hook metadata")
                .permissions()
                .mode()
                & super::UNIX_MODE_MASK;
        install_claude(&existing_config).expect("install Claude into existing hooks directory");
        let existing_mode = fs::symlink_metadata(&existing_hooks)
            .expect("existing hooks metadata")
            .permissions()
            .mode()
            & super::UNIX_MODE_MASK;
        umask(previous_umask);

        assert_eq!(new_mode, super::MANAGED_HOOK_DIR_MODE);
        assert_eq!(new_settings_mode, super::NEW_REGISTRATION_MODE);
        assert_eq!(new_hook_mode, super::MANAGED_HOOK_MODE);
        assert_eq!(
            existing_mode, 0o750,
            "installer must preserve an existing user directory"
        );
    }

    #[test]
    fn missing_claude_hooks_directory_is_reinstallable_not_installed() {
        let claude = scoped_dir("status-missing-claude-hooks-parent");

        let report = explicit_status(&claude, RuntimeRef::claude());

        assert_eq!(
            report.state,
            protocol::IntegrationInstallState::NotInstalled
        );
        assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
        assert!(report
            .warnings
            .iter()
            .all(|warning| !warning.contains("managed asset parent")));
    }

    #[test]
    fn relative_provider_config_roots_are_rejected_before_cwd_can_diverge() {
        let home = scoped_dir("relative-provider-root-home");
        let relative = Path::new("relative-agent-config");

        let claude_error = with_status_env(Some(relative), None, Some(&home), || {
            super::claude_config_dir().expect_err("reject relative Claude config root")
        });
        let codex_error = with_status_env(None, Some(relative), Some(&home), || {
            super::codex_config_dir().expect_err("reject relative Codex config root")
        });

        assert_eq!(claude_error.code, "agent_config_dir_invalid");
        assert_eq!(codex_error.code, "agent_config_dir_invalid");
        assert!(claude_error.msg.contains("absolute UTF-8 path"));
        assert!(codex_error.msg.contains("absolute UTF-8 path"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_provider_config_root_is_rejected_before_registration() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let mut bytes = b"/work/pohunek-non-utf8-".to_vec();
        bytes.push(0xff);
        let path = OsString::from_vec(bytes);
        // The launch base environment never carries a non-UTF-8 variable, so
        // the resolver is exercised with the lookup directly.
        let declared = crate::agent::host::ConfigHome::new("PROVIDER_HOME", ".provider")
            .expect("valid declaration");
        let error = super::homes::resolve_declared(&declared, &|name| {
            (name == "PROVIDER_HOME").then(|| path.clone())
        })
        .expect_err("reject non-UTF-8 config root");

        assert_eq!(error.code, "agent_config_dir_invalid");
    }

    #[cfg(unix)]
    #[test]
    fn owner_private_metadata_rejects_foreign_uid_without_chown() {
        let effective_uid = nix::unistd::Uid::effective().as_raw();
        let foreign_uid = effective_uid.wrapping_add(1);
        let metadata = super::UnixMetadataSnapshot {
            is_expected_type: true,
            owner_uid: foreign_uid,
            mode: super::MANAGED_HOOK_MODE,
        };

        assert_eq!(
            super::evaluate_owner_private_metadata(metadata, effective_uid),
            Err(super::MetadataTrustIssue::ForeignOwner {
                actual: foreign_uid,
                expected: effective_uid,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn writable_managed_asset_parents_require_manual_repair() {
        use std::os::unix::fs::PermissionsExt as _;

        for (name, agent, mode) in [
            ("codex config root", RuntimeRef::codex(), 0o775),
            ("claude hooks directory", RuntimeRef::claude(), 0o757),
        ] {
            let config_dir = scoped_dir(&format!("status-writable-parent-{name}"));
            let parent = match agent.as_wire() {
                RuntimeId::CODEX => {
                    install_codex(&config_dir).expect("install Codex fixture");
                    config_dir.clone()
                }
                RuntimeId::CLAUDE => {
                    install_claude(&config_dir).expect("install Claude fixture");
                    config_dir.join("hooks")
                }
                _ => {
                    unreachable!("test uses managed agents")
                }
            };
            fs::set_permissions(&parent, fs::Permissions::from_mode(mode))
                .expect("make managed asset parent writable");

            let report = explicit_status(&config_dir, agent);

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{name}"
            );
            assert_eq!(
                report.recovery,
                protocol::IntegrationRecovery::RepairConfiguration,
                "{name}"
            );
            assert!(
                report.warnings.iter().any(|warning| {
                    (warning.contains("managed asset parent")
                        && warning.contains(&format!("mode {mode:04o}")))
                        // A group/world-writable config root is refused by the same
                        // trusted walk the installer uses, before assets are inspected.
                        || (name == "codex config root"
                            && warning.contains("failed trusted filesystem validation"))
                }),
                "{name}: {:?}",
                report.warnings
            );
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
            let codex = scoped_dir(&format!("status-asset-{name}"));
            install_codex(&codex).expect("install Codex fixture");
            let path = codex.join(relative_path);
            match mutation {
                Some(content) => fs::write(&path, content).expect("modify managed asset"),
                None => fs::remove_file(&path).expect("remove managed asset"),
            }

            let report = explicit_status(&codex, RuntimeRef::codex());

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{name}"
            );
            assert_eq!(
                report.recovery,
                protocol::IntegrationRecovery::Reinstall,
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

    #[test]
    fn installed_version_is_unknown_when_one_readable_asset_has_no_valid_marker() {
        let codex = scoped_dir("status-invalid-version-marker");
        install_codex(&codex).expect("install Codex fixture");
        let notify_path = codex.join(NOTIFY_HOOK_INSTALL_NAME);
        let invalid_asset = CODEX_NOTIFY_HOOK_ASSET.replacen(
            &format!("{INTEGRATION_VERSION_PREFIX}{EXPECTED_INTEGRATION_VERSION}"),
            "# POHUNEK_INTEGRATION_VERSION=invalid",
            1,
        );
        fs::write(notify_path, invalid_asset).expect("write invalid marker fixture");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.installed_version, None);
        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
    }

    #[test]
    fn status_classifies_oversized_asset_as_reinstallable_outdated() {
        let codex = scoped_dir("status-oversized-asset");
        install_codex(&codex).expect("install Codex fixture");
        fs::write(
            codex.join(STATE_HOOK_INSTALL_NAME),
            vec![b'#'; super::MANAGED_ASSET_INSPECTION_LIMIT_BYTES + 1],
        )
        .expect("write oversized asset");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("exceeds the 65536-byte inspection limit")));
    }

    #[test]
    fn status_classifies_oversized_provider_config_as_manual_repair() {
        let codex = scoped_dir("status-oversized-config");
        install_codex(&codex).expect("install Codex fixture");
        fs::write(
            codex.join("config.toml"),
            vec![b' '; super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES + 1],
        )
        .expect("write oversized config");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("exceeds the 2097152-byte inspection limit")));
    }

    #[cfg(unix)]
    #[test]
    fn status_rejects_provider_config_fifo_without_blocking() {
        let codex = scoped_dir("status-config-fifo");
        install_codex(&codex).expect("install Codex fixture");
        let config_path = codex.join("config.toml");
        fs::remove_file(&config_path).expect("remove regular Codex config");
        nix::unistd::mkfifo(&config_path, nix::sys::stat::Mode::S_IRWXU)
            .expect("create the provider config FIFO");

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let inspected = codex.clone();
        thread::spawn(move || {
            let report = super::status_at(
                super::StatusAgent::Codex,
                &inspected,
                CODEX_HOOK_ASSET.as_str(),
                CODEX_NOTIFY_HOOK_ASSET.as_str(),
            );
            sender.send(report).expect("send FIFO status report");
        });
        let report = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("provider FIFO inspection must not block");

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("Codex config.toml is not a regular file")));
    }

    #[cfg(unix)]
    #[test]
    fn managed_asset_symlink_requires_repair_and_reinstall_preserves_target() {
        use std::os::unix::fs::symlink;

        let codex = scoped_dir("status-asset-symlink");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let sentinel_path = codex.join("sentinel-target");
        let sentinel = b"sentinel-private-payload";
        fs::write(&sentinel_path, sentinel).expect("write sentinel target");
        fs::remove_file(&hook_path).expect("remove managed hook");
        symlink(&sentinel_path, &hook_path).expect("link managed hook to sentinel");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        install_codex(&codex).expect("safely replace managed symlink");
        assert_eq!(
            fs::read(&sentinel_path).expect("read sentinel target"),
            sentinel,
            "managed asset replacement must not follow the symlink"
        );
        assert!(
            fs::symlink_metadata(&hook_path)
                .expect("replacement metadata")
                .file_type()
                .is_file(),
            "managed asset must become a regular file"
        );
        assert_eq!(
            fs::read_to_string(&hook_path).expect("read replacement hook"),
            *CODEX_HOOK_ASSET
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_asset_parent_symlink_requires_manual_repair() {
        use std::os::unix::fs::symlink;

        let claude = scoped_dir("status-asset-parent-symlink");
        install_claude(&claude).expect("install Claude fixture");
        let hooks_path = claude.join("hooks");
        let real_hooks_path = claude.join("hooks-real");
        fs::rename(&hooks_path, &real_hooks_path).expect("move real hooks directory");
        symlink(&real_hooks_path, &hooks_path).expect("link managed asset parent");

        let report = explicit_status(&claude, RuntimeRef::claude());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("managed asset parent")
                && warning.contains("could not be opened safely")));
    }

    #[test]
    fn managed_asset_metadata_error_requires_manual_repair() {
        let claude = scoped_dir("status-asset-metadata-error");
        fs::write(claude.join("hooks"), "not a directory\n")
            .expect("replace managed asset parent with a file");

        let report = explicit_status(&claude, RuntimeRef::claude());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("managed asset parent")));
    }

    #[cfg(unix)]
    #[test]
    fn status_rejects_unsafe_managed_hook_permissions_with_repair_hint() {
        use std::os::unix::fs::PermissionsExt;

        let codex = scoped_dir("status-unsafe-hook-permissions");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let mut permissions = fs::metadata(&hook_path)
            .expect("managed hook metadata")
            .permissions();
        permissions.set_mode(0o777);
        fs::set_permissions(&hook_path, permissions).expect("make managed hook unsafe");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
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

    #[cfg(unix)]
    #[test]
    fn status_detects_special_managed_hook_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        let codex = scoped_dir("status-special-hook-mode");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o4755))
            .expect("set managed hook setuid bit");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("mode 4755")));
    }

    #[cfg(unix)]
    #[test]
    fn reinstall_repairs_managed_hook_permission_drift() {
        use std::os::unix::fs::PermissionsExt as _;

        for (index, drifted_mode) in [0o600, 0o777, 0o4755].into_iter().enumerate() {
            let codex = scoped_dir(&format!("reinstall-hook-mode-{index}"));
            install_codex(&codex).expect("install Codex fixture");
            let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
            fs::set_permissions(&hook_path, fs::Permissions::from_mode(drifted_mode))
                .expect("drift managed hook permissions");

            install_codex(&codex).expect("reinstall must repair managed hook permissions");

            assert_eq!(
                fs::metadata(&hook_path)
                    .expect("inspect repaired hook")
                    .permissions()
                    .mode()
                    & super::UNIX_MODE_MASK,
                super::MANAGED_HOOK_MODE
            );
            assert_eq!(
                fs::read(&hook_path).expect("read repaired hook"),
                include_bytes!("reporter_golden/codex-pohunek-agent-state.sh")
            );
        }
    }

    #[test]
    fn status_detects_broken_claude_registration() {
        let claude = scoped_dir("status-claude-registration");
        install_claude(&claude).expect("install Claude fixture");
        let settings_path = claude.join("settings.json");
        let mut settings = read_json(&settings_path);
        settings["hooks"][SESSION_START_EVENT][0]["hooks"][0]["timeout"] = json!(1);
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).expect("serialize modified settings"),
        )
        .expect("write modified settings");

        let report = explicit_status(&claude, RuntimeRef::claude());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("Claude SessionStart registration")));
    }

    #[test]
    fn status_rejects_managed_registration_under_an_extra_event() {
        let codex = scoped_dir("status-extra-registration-event");
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

        let report = explicit_status(&codex, RuntimeRef::codex());

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
    fn codex_managed_hook_group_with_sibling_handler_is_not_current() {
        let codex = scoped_dir("status-codex-sibling-handler");
        install_codex(&codex).expect("install Codex fixture");
        let hooks_path = codex.join("hooks.json");
        let mut hooks = read_json(&hooks_path);
        hooks["hooks"][SESSION_START_EVENT][0]["hooks"]
            .as_array_mut()
            .expect("managed Codex handler group")
            .push(json!({
                "type": "command",
                "command": "echo user-sibling",
                "timeout": HOOK_TIMEOUT_SECS,
            }));
        fs::write(
            &hooks_path,
            serde_json::to_vec_pretty(&hooks).expect("serialize sibling handler fixture"),
        )
        .expect("write sibling handler fixture");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("Codex SessionStart registration")));

        install_codex(&codex).expect("normalize managed Codex group");
        let repaired = explicit_status(&codex, RuntimeRef::codex());
        assert_eq!(repaired.state, protocol::IntegrationInstallState::Current);
        let repaired_hooks = read_json(&hooks_path);
        assert_eq!(
            super::hook_command_count(
                repaired_hooks["hooks"]
                    .as_object()
                    .expect("repaired hooks object"),
                "echo user-sibling",
            ),
            1,
            "reinstall must preserve the sibling user handler"
        );
    }

    #[test]
    fn stale_codex_managed_trust_keys_are_reinstalled_to_the_exact_set() {
        let codex = scoped_dir("status-codex-stale-trust-key");
        install_codex(&codex).expect("install Codex fixture");
        let hooks_path = codex.join("hooks.json");
        let config_path = codex.join("config.toml");
        let stale_key =
            super::codex_hook_trust_key(&hooks_path, CODEX_SESSION_START_TRUST_EVENT, 99, 99);
        let managed_hash = codex_command_hook_trusted_hash(
            CODEX_SESSION_START_TRUST_EVENT,
            &hook_command(&codex.join(STATE_HOOK_INSTALL_NAME), super::HOOK_ACTION),
            HOOK_TIMEOUT_SECS,
            None,
        )
        .expect("managed hook trust hash");
        let mut config = fs::read_to_string(&config_path).expect("read Codex config");
        write!(
            config,
            "\n[hooks.state.{}]\ntrusted_hash = \"{managed_hash}\"\n",
            toml_basic_string(&stale_key)
        )
        .expect("append stale trust table");
        fs::write(&config_path, config).expect("append stale trust entry");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("stale or unexpected keys")));

        install_codex(&codex).expect("remove stale managed trust table");
        let repaired = explicit_status(&codex, RuntimeRef::codex());
        let repaired_config = fs::read_to_string(&config_path).expect("read repaired config");
        assert_eq!(repaired.state, protocol::IntegrationInstallState::Current);
        assert!(!repaired_config.contains(&stale_key));
    }

    #[test]
    fn invalid_hook_event_structure_requires_manual_repair_before_install() {
        let codex = scoped_dir("status-invalid-hook-event");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let sentinel = "# managed asset sentinel\n";
        fs::write(&hook_path, sentinel).expect("write managed asset sentinel");
        let hooks_path = codex.join("hooks.json");
        let mut hooks = read_json(&hooks_path);
        hooks["hooks"][SESSION_START_EVENT] = json!({ "invalid": true });
        fs::write(
            &hooks_path,
            serde_json::to_vec_pretty(&hooks).expect("serialize invalid hooks structure"),
        )
        .expect("write invalid hooks structure");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        let error = install_codex(&codex).expect_err("invalid event structure must fail install");
        assert_eq!(error.code, "integration_settings_invalid");
        assert_eq!(
            fs::read_to_string(&hook_path).expect("read managed asset sentinel"),
            sentinel,
            "config validation must precede managed asset replacement"
        );
    }

    #[test]
    fn non_object_registration_root_requires_manual_repair_for_each_provider() {
        for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
            let config_dir = scoped_dir(&format!("status-non-object-root-{}", agent.as_wire()));
            let registration_path = match agent.as_wire() {
                RuntimeId::CLAUDE => {
                    install_claude(&config_dir).expect("install Claude fixture");
                    config_dir.join("settings.json")
                }
                RuntimeId::CODEX => {
                    install_codex(&config_dir).expect("install Codex fixture");
                    config_dir.join("hooks.json")
                }
                _ => panic!("test uses only managed hook agents"),
            };
            fs::write(&registration_path, "[]\n").expect("write non-object registration root");

            let report = explicit_status(&config_dir, agent.clone());

            assert_eq!(
                report.recovery,
                protocol::IntegrationRecovery::RepairConfiguration,
                "{}",
                agent.as_wire()
            );
            let error = match agent.as_wire() {
                RuntimeId::CLAUDE => install_claude(&config_dir),
                RuntimeId::CODEX => install_codex(&config_dir),
                _ => unreachable!("test uses only managed hook agents"),
            }
            .expect_err("non-object registration root must fail install");
            assert_eq!(error.code, "integration_settings_invalid");
        }
    }

    #[test]
    fn missing_hooks_object_is_reinstallable_for_each_provider() {
        for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
            let config_dir = scoped_dir(&format!("status-missing-hooks-{}", agent.as_wire()));
            let registration_path = match agent.as_wire() {
                RuntimeId::CLAUDE => {
                    install_claude(&config_dir).expect("install Claude fixture");
                    config_dir.join("settings.json")
                }
                RuntimeId::CODEX => {
                    install_codex(&config_dir).expect("install Codex fixture");
                    config_dir.join("hooks.json")
                }
                _ => panic!("test uses only managed hook agents"),
            };
            fs::write(registration_path, "{}\n").expect("remove hooks object");

            let report = explicit_status(&config_dir, agent.clone());

            assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
            assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
            match agent.as_wire() {
                RuntimeId::CLAUDE => install_claude(&config_dir),
                RuntimeId::CODEX => install_codex(&config_dir),
                _ => unreachable!("test uses only managed hook agents"),
            }
            .expect("reinstall missing hooks object");
            assert_eq!(
                explicit_status(&config_dir, agent).state,
                protocol::IntegrationInstallState::Current
            );
        }
    }

    #[test]
    fn reinstall_replaces_owned_command_with_missing_type_for_each_provider() {
        for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
            let config_dir = scoped_dir(&format!("status-owned-command-{}", agent.as_wire()));
            let registration_path = match agent.as_wire() {
                RuntimeId::CLAUDE => {
                    install_claude(&config_dir).expect("install Claude fixture");
                    config_dir.join("settings.json")
                }
                RuntimeId::CODEX => {
                    install_codex(&config_dir).expect("install Codex fixture");
                    config_dir.join("hooks.json")
                }
                _ => panic!("test uses only managed hook agents"),
            };
            let mut registration = read_json(&registration_path);
            registration["hooks"][SESSION_START_EVENT][0]["hooks"][0]
                .as_object_mut()
                .expect("managed handler object")
                .remove("type");
            fs::write(
                &registration_path,
                serde_json::to_vec_pretty(&registration).expect("serialize drifted registration"),
            )
            .expect("write drifted registration");

            let report = explicit_status(&config_dir, agent.clone());

            assert_eq!(report.recovery, protocol::IntegrationRecovery::Reinstall);
            match agent.as_wire() {
                RuntimeId::CLAUDE => install_claude(&config_dir),
                RuntimeId::CODEX => install_codex(&config_dir),
                _ => unreachable!("test uses only managed hook agents"),
            }
            .expect("reinstall exact owned command identity");
            let repaired = explicit_status(&config_dir, agent);
            assert_eq!(repaired.state, protocol::IntegrationInstallState::Current);
            assert!(repaired.warnings.is_empty(), "{:?}", repaired.warnings);
        }
    }

    #[test]
    fn invalid_codex_config_structure_requires_manual_repair_before_install() {
        let codex = scoped_dir("status-invalid-config-structure");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let sentinel = "# managed asset sentinel\n";
        fs::write(&hook_path, sentinel).expect("write managed asset sentinel");
        fs::write(codex.join("config.toml"), "features = true\n")
            .expect("write invalid Codex config structure");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        let error = install_codex(&codex).expect_err("invalid config structure must fail install");
        assert_eq!(error.code, "integration_settings_invalid");
        assert_eq!(
            fs::read_to_string(&hook_path).expect("read managed asset sentinel"),
            sentinel,
            "config validation must precede managed asset replacement"
        );
    }

    #[test]
    fn scalar_managed_trust_key_requires_manual_repair_before_install() {
        let codex = scoped_dir("status-scalar-trust-key");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let sentinel = "# managed asset sentinel\n";
        fs::write(&hook_path, sentinel).expect("write managed asset sentinel");
        let trust_key = codex_hook_trust_key(
            &codex.join("hooks.json"),
            CODEX_SESSION_START_TRUST_EVENT,
            0,
            0,
        );
        fs::write(
            codex.join("config.toml"),
            format!(
                "[features]\nhooks = true\n\n[hooks.state]\n{} = \"invalid\"\n",
                toml_basic_string(&trust_key)
            ),
        )
        .expect("write scalar managed trust key");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        let error = install_codex(&codex).expect_err("scalar trust key must fail install");
        assert_eq!(error.code, "integration_settings_invalid");
        assert_eq!(
            fs::read_to_string(&hook_path).expect("read managed asset sentinel"),
            sentinel,
            "config validation must precede managed asset replacement"
        );
    }

    #[test]
    fn scalar_future_trust_key_with_hook_drift_requires_manual_repair() {
        let codex = scoped_dir("status-scalar-future-trust-key");
        install_codex(&codex).expect("install Codex fixture");
        let hook_path = codex.join(STATE_HOOK_INSTALL_NAME);
        let sentinel = "# managed asset sentinel\n";
        fs::write(&hook_path, sentinel).expect("write managed asset sentinel");
        let hooks_path = codex.join("hooks.json");
        let mut hooks = read_json(&hooks_path);
        hooks["hooks"][SESSION_START_EVENT][0]["hooks"]
            .as_array_mut()
            .expect("managed Codex handler group")
            .push(json!({
                "type": "command",
                "command": "echo user-sibling",
            }));
        fs::write(
            &hooks_path,
            serde_json::to_vec_pretty(&hooks).expect("serialize drifted Codex hooks"),
        )
        .expect("write drifted Codex hooks");

        let config_path = codex.join("config.toml");
        // Reinstall preserves the sibling as group zero and recreates the
        // managed handler at this future position.
        let trust_key = codex_hook_trust_key(&hooks_path, CODEX_SESSION_START_TRUST_EVENT, 1, 0);
        let mut config = fs::read_to_string(&config_path)
            .expect("read Codex config")
            .parse::<toml_edit::DocumentMut>()
            .expect("parse Codex config");
        config["hooks"]["state"]
            .as_table_mut()
            .expect("Codex trust state table")
            .insert(&trust_key, toml_edit::value("sha256:invalid"));
        fs::write(&config_path, config.to_string()).expect("write scalar future trust key");

        let report = explicit_status(&codex, RuntimeRef::codex());

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        let error = install_codex(&codex).expect_err("scalar future trust key must fail install");
        assert_eq!(error.code, "integration_settings_invalid");
        assert_eq!(
            fs::read_to_string(&hook_path).expect("read managed asset sentinel"),
            sentinel,
            "config validation must precede managed asset replacement"
        );
    }

    #[test]
    fn status_detects_broken_codex_registration_trust_and_read_errors() {
        for failure in ["registration", "trust", "read", "malformed-config"] {
            let codex = scoped_dir(&format!("status-codex-{failure}"));
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
                "malformed-config" => {
                    fs::write(codex.join("config.toml"), "[invalid")
                        .expect("write malformed config");
                }
                other => panic!("unknown fixture failure: {other}"),
            }

            let report = explicit_status(&codex, RuntimeRef::codex());

            assert_eq!(
                report.state,
                protocol::IntegrationInstallState::Outdated,
                "{failure}"
            );
            assert!(!report.warnings.is_empty(), "{failure}");
            let expected_recovery = if matches!(failure, "read" | "malformed-config") {
                protocol::IntegrationRecovery::RepairConfiguration
            } else {
                protocol::IntegrationRecovery::Reinstall
            };
            assert_eq!(report.recovery, expected_recovery, "{failure}");
        }
    }

    #[test]
    fn aggregate_status_degrades_one_resolution_failure_without_aborting() {
        let codex = scoped_dir("status-aggregate-resolve");
        install_codex(&codex).expect("install Codex fixture");

        let result = with_status_env(None, Some(&codex), None, || {
            super::status(protocol::IntegrationStatusParams {
                agent: None,
                ..Default::default()
            })
        })
        .expect("aggregate status");

        assert_eq!(result.agents.len(), 2);
        assert_eq!(result.agents[0].agent, RuntimeRef::claude());
        assert_eq!(
            result.agents[0].state,
            protocol::IntegrationInstallState::Outdated
        );
        assert!(result.agents[0].warnings[0].contains("missing_env"));
        assert_eq!(
            result.agents[0].recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert_eq!(result.agents[1].agent, RuntimeRef::codex());
        assert_eq!(
            result.agents[1].state,
            protocol::IntegrationInstallState::Current
        );
    }

    #[test]
    fn explicit_status_degrades_resolution_failure_to_warning() {
        let report = with_status_env(None, None, None, || {
            super::status(protocol::IntegrationStatusParams {
                agent: Some(RuntimeRef::claude()),
                ..Default::default()
            })
        })
        .expect("explicit status")
        .agents
        .pop()
        .expect("Claude report");

        assert_eq!(report.state, protocol::IntegrationInstallState::Outdated);
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert!(report.warnings[0].contains("missing_env"));
    }

    #[test]
    fn explicit_unsupported_status_and_install_agents_return_typed_errors() {
        for agent in [
            RuntimeRef::shell(),
            RuntimeRef::hermes(),
            RuntimeRef::from_wire("pi"),
        ] {
            let status = super::status(protocol::IntegrationStatusParams {
                agent: Some(agent.clone()),
                ..Default::default()
            })
            .expect_err("status must reject the agent");
            assert_eq!(status.code, "agent_not_installable", "{agent:?} status");
            let install =
                super::install(Some(agent.clone())).expect_err("install must reject the agent");
            assert_eq!(install.code, "agent_not_installable", "{agent:?} install");
        }
    }

    /// The `PATH` a hook child inherits, read under the process-environment lock
    /// so a test that overrides `PATH` cannot leak its value into the child.
    pub(super) fn inherited_path() -> String {
        let _env = ProcessEnv::lock();
        std::env::var("PATH").unwrap_or_default()
    }

    pub(super) fn with_config_dirs<T>(
        claude_dir: &Path,
        codex_dir: &Path,
        operation: impl FnOnce() -> T,
    ) -> T {
        with_config_dirs_and(claude_dir, codex_dir, &[], operation)
    }

    /// Like [`with_config_dirs`], additionally setting `extra` variables for the
    /// duration of `operation`.
    pub(super) fn with_config_dirs_and<T>(
        claude_dir: &Path,
        codex_dir: &Path,
        extra: &[(&str, &str)],
        operation: impl FnOnce() -> T,
    ) -> T {
        let mut env = ProcessEnv::lock();
        env.set(super::CLAUDE_CONFIG_DIR_ENV, claude_dir)
            .set(super::CODEX_HOME_ENV, codex_dir);
        for (key, value) in extra {
            env.set(key, value);
        }
        operation()
    }

    pub(super) fn with_status_env<T>(
        claude_dir: Option<&Path>,
        codex_dir: Option<&Path>,
        home: Option<&Path>,
        operation: impl FnOnce() -> T,
    ) -> T {
        let mut env = ProcessEnv::lock();
        set_optional_env(&mut env, super::CLAUDE_CONFIG_DIR_ENV, claude_dir);
        set_optional_env(&mut env, super::CODEX_HOME_ENV, codex_dir);
        set_optional_env(&mut env, "HOME", home);
        operation()
    }

    fn set_optional_env(env: &mut ProcessEnv, key: &str, value: Option<&Path>) {
        match value {
            Some(value) => env.set(key, value),
            None => env.remove(key),
        };
    }

    pub(super) fn explicit_status(
        config_dir: &Path,
        agent: RuntimeRef,
    ) -> protocol::IntegrationAgentStatus {
        with_config_dirs(config_dir, config_dir, || {
            super::status(protocol::IntegrationStatusParams {
                agent: Some(agent),
                ..Default::default()
            })
        })
        .expect("explicit status")
        .agents
        .into_iter()
        .next()
        .expect("one agent report")
    }

    pub(super) fn tree_snapshot(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
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
        let claude_dir = scoped_dir("claude-merge");
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
        let codex_dir = scoped_dir("codex-fresh");
        let paths = install_codex(&codex_dir).expect("install codex");

        assert!(paths.hook_path.is_file());
        let hooks = read_json(&codex_dir.join("hooks.json"));
        let commands = session_start_command_hooks(&hooks);
        assert_eq!(commands.len(), 1);
        // Codex SessionStart hook carries no matcher key.
        assert!(hooks["hooks"]["SessionStart"][0].get("matcher").is_none());
        for (event, action) in [
            (SUBAGENT_START_EVENT, SUBAGENT_START_ACTION),
            (SUBAGENT_STOP_EVENT, SUBAGENT_STOP_ACTION),
        ] {
            let commands = command_hooks(&hooks, event);
            assert_eq!(commands.len(), 1, "exactly one {event} hook");
            assert!(commands[0].ends_with(action));
        }

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
        let codex_dir = scoped_dir("codex-idem");
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
        let codex_dir = scoped_dir("codex-dotted");
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
        let codex_dir = scoped_dir("codex-inline");
        fs::write(
            codex_dir.join("config.toml"),
            "features = { hooks = false }\n",
        )
        .unwrap();

        let report = explicit_status(&codex_dir, RuntimeRef::codex());
        let err = install_codex(&codex_dir).expect_err("inline features table is refused");

        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
        assert_eq!(err.code, "integration_settings_invalid");
        assert!(
            err.msg.contains("features") && err.msg.contains("not a TOML table"),
            "{err:?}"
        );
    }

    #[test]
    fn install_claude_preserves_user_hook_that_mentions_managed_notify_path() {
        let claude_dir = scoped_dir("claude-substring-owned");
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
        let codex_dir = scoped_dir("codex-substring-owned");
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
        let claude_dir = scoped_dir("claude-notify-install");
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
        let codex_dir = scoped_dir("codex-notify-install");
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
        let parent = scoped_dir("missing-parent");
        let missing = parent.join("does-not-exist");
        let err = install_claude(&missing).expect_err("missing claude dir");
        assert_eq!(err.code, "agent_config_dir_missing");
        let err = install_codex(&missing).expect_err("missing codex dir");
        assert_eq!(err.code, "agent_config_dir_missing");
    }
}
