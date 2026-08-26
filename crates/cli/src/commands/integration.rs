//! `pohunek integration` lifecycle commands.
//!
//! Codex and Claude hook installation remains a local daemon RPC. Hermes is an
//! owner-local plugin lifecycle and deliberately never contacts the daemon.

// Rust guideline compliant 2026-08-12

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use protocol::{
    method, AgentKind, ErrorClass, IntegrationInstallParams, IntegrationInstallResult,
    IntegrationInstallState, IntegrationStatusParams, IntegrationStatusResult, ProtocolError,
};
use serde::Serialize;

use crate::client::Client;
use crate::error::CliError;
use crate::hermes_integration::doctor;
use crate::hermes_integration::error::Error as HermesError;
use crate::hermes_integration::lifecycle::{
    self, InstallRequest, LifecycleState, UninstallRequest,
};
use crate::hermes_integration::policy::{
    self, AccessMode, Policy, PolicyInput, WildcardConfirmation, DEFAULT_REQUEST_TIMEOUT_MS,
    MAX_CONCURRENCY, MAX_OUTPUT_BYTES, MAX_SCREEN_BYTES, MAX_TIMEOUT_MS,
};
use crate::hermes_integration::runner::HermesRunner;
use crate::hermes_integration::target::{ProfileName, TargetContext, TargetSelection};
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// The maximum number of absolute PATH entries examined for the Hermes binary.
///
/// This bounds attacker-controlled environment parsing while preserving normal
/// shell layouts; relative PATH entries are never considered.
const MAX_ABSOLUTE_PATH_ENTRIES: usize = 64;
/// The executable basename accepted from an absolute PATH entry.
const HERMES_EXECUTABLE_NAME: &str = "hermes";
/// The only home environment input used to derive the default Hermes root.
const HOME_ENVIRONMENT_VARIABLE: &str = "HOME";

/// Agent selector accepted by `integration --agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum HookAgentArg {
    /// Install or manage the Hermes operator plugin locally.
    Hermes,
    /// Install the Claude Code hook.
    Claude,
    /// Install the Codex hook.
    Codex,
}

impl From<HookAgentArg> for AgentKind {
    fn from(value: HookAgentArg) -> Self {
        match value {
            HookAgentArg::Claude => AgentKind::Claude,
            HookAgentArg::Codex => AgentKind::Codex,
            HookAgentArg::Hermes => AgentKind::Hermes,
        }
    }
}

/// Explicit access mode parsed from the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AccessModeArg {
    /// Permit observation-only tools.
    #[value(name = "read_only")]
    ReadOnly,
    /// Permit observation and constrained session management.
    Manage,
    /// Permit every registered tool.
    Full,
}

impl From<AccessModeArg> for AccessMode {
    fn from(value: AccessModeArg) -> Self {
        match value {
            AccessModeArg::ReadOnly => AccessMode::ReadOnly,
            AccessModeArg::Manage => AccessMode::Manage,
            AccessModeArg::Full => AccessMode::Full,
        }
    }
}

/// One explicitly supplied Hermes target and executable selection.
#[derive(Debug, Clone)]
pub(crate) struct HermesOptions {
    /// Named Hermes profile, if selected.
    pub(crate) profile: Option<String>,
    /// Absolute custom Hermes home, if selected.
    pub(crate) home: Option<PathBuf>,
    /// Explicit fixed Hermes executable, if selected.
    pub(crate) hermes_bin: Option<PathBuf>,
    /// Explicit fixed Pohunek executable, if selected.
    pub(crate) pohunek_bin: Option<PathBuf>,
    /// Optional replacement access mode for install or update.
    pub(crate) access_mode: Option<AccessModeArg>,
    /// Optional replacement host allowlist for install or update.
    pub(crate) allowed_hosts: Vec<String>,
    /// Optional per-tool timeout for install or update.
    pub(crate) tool_timeout_ms: Option<u32>,
    /// Optional session-creation response timeout for install or update.
    pub(crate) request_timeout_ms: Option<u32>,
    /// Optional maximum tool-output size for install or update.
    pub(crate) max_output_bytes: Option<u32>,
    /// Optional maximum terminal-screen size for install or update.
    pub(crate) max_screen_bytes: Option<u32>,
    /// Optional maximum concurrent tool count for install or update.
    pub(crate) max_concurrency: Option<u8>,
    /// Explicit acknowledgement for a newly supplied wildcard host.
    pub(crate) confirm_wildcard: bool,
    /// Explicit acknowledgement for modifying or removing changed managed files.
    pub(crate) confirm_modified: bool,
}

impl HermesOptions {
    /// Whether this invocation supplied any Hermes-only setting.
    #[must_use]
    pub(crate) fn is_explicit(&self) -> bool {
        self.profile.is_some()
            || self.home.is_some()
            || self.hermes_bin.is_some()
            || self.pohunek_bin.is_some()
            || self.access_mode.is_some()
            || !self.allowed_hosts.is_empty()
            || self.tool_timeout_ms.is_some()
            || self.request_timeout_ms.is_some()
            || self.max_output_bytes.is_some()
            || self.max_screen_bytes.is_some()
            || self.max_concurrency.is_some()
            || self.confirm_wildcard
            || self.confirm_modified
    }

    fn validate_for_action(&self, action: HermesAction) -> Result<(), CliError> {
        if self.profile.is_some() == self.home.is_some() {
            return Err(hermes_usage(
                "select exactly one of `--hermes-profile` or `--hermes-home`",
            ));
        }
        let unsupported = match action {
            HermesAction::Install | HermesAction::Update => false,
            HermesAction::Status | HermesAction::Doctor => {
                self.pohunek_bin.is_some()
                    || self.access_mode.is_some()
                    || !self.allowed_hosts.is_empty()
                    || self.tool_timeout_ms.is_some()
                    || self.request_timeout_ms.is_some()
                    || self.max_output_bytes.is_some()
                    || self.max_screen_bytes.is_some()
                    || self.max_concurrency.is_some()
                    || self.confirm_wildcard
                    || self.confirm_modified
            }
            HermesAction::Uninstall => {
                self.pohunek_bin.is_some()
                    || self.access_mode.is_some()
                    || !self.allowed_hosts.is_empty()
                    || self.tool_timeout_ms.is_some()
                    || self.request_timeout_ms.is_some()
                    || self.max_output_bytes.is_some()
                    || self.max_screen_bytes.is_some()
                    || self.max_concurrency.is_some()
                    || self.confirm_wildcard
            }
        };
        if unsupported {
            return Err(hermes_usage(
                "the supplied option is not valid for this Hermes action",
            ));
        }
        if action == HermesAction::Install
            && (self.access_mode.is_none() || self.allowed_hosts.is_empty())
        {
            return Err(hermes_usage(
                "Hermes install requires `--access-mode` and at least one `--allow-host`",
            ));
        }
        Ok(())
    }
}

/// A Hermes lifecycle operation dispatched entirely in the CLI process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HermesAction {
    /// Install the managed plugin and its explicit policy.
    Install,
    /// Inspect the managed plugin lifecycle state.
    Status,
    /// Run the deterministic diagnostic inventory.
    Doctor,
    /// Atomically replace the managed plugin policy and assets.
    Update,
    /// Remove only marker-owned managed files.
    Uninstall,
}

impl HermesAction {
    fn label(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Status => "status",
            Self::Doctor => "doctor",
            Self::Update => "update",
            Self::Uninstall => "uninstall",
        }
    }
}

/// Run the original daemon-backed Codex or Claude hook installation.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request, or
/// returns an unexpected payload.
pub(crate) async fn run_install(
    paths: &Paths,
    agent: Option<HookAgentArg>,
    json: bool,
) -> Result<(), CliError> {
    let params = IntegrationInstallParams {
        agent: agent.map(Into::into),
    };
    // Installing hooks is inherently a *local* daemon operation: it writes into
    // this machine's agent config dirs. Always use the local transport regardless
    // of any `--host` flag.
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    let result: IntegrationInstallResult =
        client.call::<method::IntegrationInstall>(params).await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_install_human(&result));
    }
    Ok(())
}

/// Run the read-only Codex and Claude integration status RPC.
///
/// # Errors
///
/// Returns [`CliError`] if the local daemon is unreachable or rejects the call.
pub(crate) async fn run_status(
    paths: &Paths,
    agent: Option<HookAgentArg>,
    json: bool,
) -> Result<(), CliError> {
    let params = IntegrationStatusParams {
        agent: agent.map(Into::into),
    };
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    let result: IntegrationStatusResult = client.call::<method::IntegrationStatus>(params).await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_status_human(&result));
    }
    Ok(())
}

/// Runs a Hermes lifecycle operation without resolving the daemon runtime path.
///
/// # Errors
///
/// Returns a stable typed error before filesystem mutation when an explicit
/// input is missing or unsafe, and maps lifecycle failures without paths or
/// child-process output.
pub(crate) fn run_hermes(
    action: HermesAction,
    options: &HermesOptions,
    json: bool,
) -> Result<bool, CliError> {
    options.validate_for_action(action)?;
    let target = resolve_target(options)?;
    let policy_path = policy_path(&target)?;
    let mut runner = HermesRunner::new(&resolve_hermes_executable(options.hermes_bin.as_deref())?)
        .map_err(hermes_error)?;

    let result = match action {
        HermesAction::Install => {
            let policy = install_policy(options)?;
            let lifecycle = lifecycle::install(
                &mut runner,
                &InstallRequest::new(&target, &policy_path, &policy, options.confirm_modified),
            )
            .map_err(hermes_error)?;
            result(action, &target, lifecycle, Some(&policy), None)
        }
        HermesAction::Status => {
            let lifecycle =
                lifecycle::inspect(&mut runner, &target, &policy_path).map_err(hermes_error)?;
            let policy = lifecycle
                .installed
                .then(|| Policy::load_private(&policy_path))
                .transpose()
                .map_err(hermes_error)?;
            result(action, &target, lifecycle, policy.as_ref(), None)
        }
        HermesAction::Doctor => {
            let report = doctor::inspect(&mut runner, &target, &policy_path);
            let lifecycle = lifecycle_from_doctor(&report);
            result(action, &target, lifecycle, None, Some(report))
        }
        HermesAction::Update => {
            let existing = Policy::load_private(&policy_path).map_err(hermes_error)?;
            let policy = update_policy(options, &existing)?;
            let lifecycle = lifecycle::install(
                &mut runner,
                &InstallRequest::new(&target, &policy_path, &policy, options.confirm_modified),
            )
            .map_err(hermes_error)?;
            result(action, &target, lifecycle, Some(&policy), None)
        }
        HermesAction::Uninstall => {
            let lifecycle = lifecycle::uninstall(
                &mut runner,
                &UninstallRequest::new(&target, &policy_path, options.confirm_modified),
            )
            .map_err(hermes_error)?;
            result(action, &target, lifecycle, None, None)
        }
    };

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_hermes_human(&result));
    }
    Ok(result.doctor.as_ref().is_none_or(|report| report.ok))
}

/// Returns the typed unsupported-action error before any daemon connection.
pub(crate) fn unsupported_action(agent: Option<HookAgentArg>) -> CliError {
    let agent = agent.map_or("none", agent_name);
    CliError::Protocol(ProtocolError::new(
        ErrorClass::Configuration,
        "integration_action_unsupported",
        format!("integration lifecycle action is supported only for Hermes, not {agent}"),
        Some("pass `--agent hermes` for status, doctor, update, or uninstall".to_owned()),
    ))
}

/// Returns the typed error for Hermes-only flags on a non-Hermes install.
pub(crate) fn hermes_options_require_hermes() -> CliError {
    CliError::Protocol(ProtocolError::new(
        ErrorClass::Configuration,
        "integration_hermes_options_require_hermes",
        "Hermes integration options require `--agent hermes`".to_owned(),
        Some("pass `--agent hermes` or remove Hermes-specific options".to_owned()),
    ))
}

fn resolve_target(
    options: &HermesOptions,
) -> Result<crate::hermes_integration::target::ResolvedTarget, CliError> {
    let home = env::var_os(HOME_ENVIRONMENT_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CliError::MissingEnv {
            var: HOME_ENVIRONMENT_VARIABLE.to_owned(),
        })?;
    let context = TargetContext::new(
        home.join(".hermes"),
        home,
        nearest_git_workspace().into_iter().collect(),
    )
    .map_err(hermes_error)?;
    let selection = match (&options.profile, &options.home) {
        (Some(profile), None) => {
            TargetSelection::Profile(ProfileName::new(profile.clone()).map_err(hermes_error)?)
        }
        (None, Some(home)) => TargetSelection::CustomHome(home.clone()),
        (None, None) => {
            return Err(hermes_usage(
                "select exactly one of `--hermes-profile` or `--hermes-home`",
            ))
        }
        (Some(_), Some(_)) => return Err(hermes_error(HermesError::UnsafeTarget)),
    };
    context.resolve(selection).map_err(hermes_error)
}

fn hermes_usage(message: &str) -> CliError {
    CliError::Protocol(ProtocolError::new(
        ErrorClass::Configuration,
        "integration_hermes_usage",
        message.to_owned(),
        Some("run `pohunek integration <action> --help` for the action-specific syntax".to_owned()),
    ))
}

fn nearest_git_workspace() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    cwd.ancestors().find_map(|candidate| {
        fs::symlink_metadata(candidate.join(".git"))
            .ok()
            .and_then(|marker| (marker.is_dir() || marker.is_file()).then(|| candidate.to_owned()))
    })
}

fn policy_path(
    target: &crate::hermes_integration::target::ResolvedTarget,
) -> Result<PathBuf, CliError> {
    let state_home = pohunek_paths::state_home().map_err(|error| match error {
        pohunek_paths::PathError::MissingEnv { var } => CliError::MissingEnv { var },
    })?;
    policy::policy_path(&state_home.join(pohunek_paths::APP_DIR), target).map_err(hermes_error)
}

fn resolve_hermes_executable(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        if !path.is_absolute() {
            return Err(hermes_error(HermesError::RelativePath));
        }
        return Ok(path.to_owned());
    }
    let Some(path) = env::var_os("PATH") else {
        return Err(hermes_error(HermesError::InvalidHermesExecutable));
    };
    resolve_hermes_from_path(&path)
}

fn resolve_hermes_from_path(path: &std::ffi::OsStr) -> Result<PathBuf, CliError> {
    for entry in env::split_paths(path)
        .filter(|entry| entry.is_absolute())
        .take(MAX_ABSOLUTE_PATH_ENTRIES)
    {
        let candidate = entry.join(HERMES_EXECUTABLE_NAME);
        if fs::symlink_metadata(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err(hermes_error(HermesError::InvalidHermesExecutable))
}

fn install_policy(options: &HermesOptions) -> Result<Policy, CliError> {
    let access_mode = options
        .access_mode
        .ok_or_else(|| hermes_error(HermesError::InvalidPolicy))?;
    if options.allowed_hosts.is_empty() {
        return Err(hermes_error(HermesError::InvalidPolicy));
    }
    new_policy(
        options,
        access_mode.into(),
        options.allowed_hosts.clone(),
        options.confirm_wildcard,
        None,
    )
}

fn update_policy(options: &HermesOptions, existing: &Policy) -> Result<Policy, CliError> {
    let access_mode = options
        .access_mode
        .map_or_else(|| existing.access_mode(), Into::into);
    let supplied_hosts = !options.allowed_hosts.is_empty();
    let hosts = if supplied_hosts {
        options.allowed_hosts.clone()
    } else {
        existing.allowed_hosts().map(str::to_owned).collect()
    };
    new_policy(
        options,
        access_mode,
        hosts,
        // Existing wildcard entries were explicitly confirmed at install time;
        // only a replacement host list must acknowledge a wildcard again.
        !supplied_hosts || options.confirm_wildcard,
        Some(existing),
    )
}

fn new_policy(
    options: &HermesOptions,
    access_mode: AccessMode,
    allowed_hosts: Vec<String>,
    confirm_wildcard: bool,
    existing: Option<&Policy>,
) -> Result<Policy, CliError> {
    let pohunek_cli = match (options.pohunek_bin.clone(), existing) {
        (Some(path), _) => path,
        (None, Some(policy)) => policy.pohunek_cli().to_owned(),
        (None, None) => env::current_exe()
            .map_err(HermesError::from)
            .map_err(hermes_error)?,
    };
    // Policies always use this binary's range so updates repair drift that
    // `assets/pohunek/cli.py::_validate_envelope` rejects as `pohunek_cli_incompatible`.
    let versions = protocol::SUPPORTED_PROTOCOL_VERSIONS;
    let protocol_min = i32::try_from(versions.minimum().get())
        .map_err(|_error| hermes_error(HermesError::InvalidPolicy))?;
    let protocol_max = i32::try_from(versions.maximum().get())
        .map_err(|_error| hermes_error(HermesError::InvalidPolicy))?;
    if existing.is_some_and(|policy| {
        policy.protocol_min() != protocol_min || policy.protocol_max() != protocol_max
    }) {
        tracing::info!(
            name: "hermes.integration.protocol_range.refresh",
            protocol_min,
            protocol_max,
            "refreshing stored Hermes policy protocol range"
        );
    }
    Policy::new(PolicyInput {
        pohunek_cli,
        protocol_min,
        protocol_max,
        access_mode,
        allowed_hosts,
        tool_timeout_ms: options
            .tool_timeout_ms
            .or_else(|| existing.map(Policy::tool_timeout_ms))
            .unwrap_or(MAX_TIMEOUT_MS),
        request_timeout_ms: options
            .request_timeout_ms
            .or_else(|| existing.map(Policy::request_timeout_ms))
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        max_output_bytes: options
            .max_output_bytes
            .or_else(|| existing.map(Policy::max_output_bytes))
            .unwrap_or(MAX_OUTPUT_BYTES),
        max_screen_bytes: options
            .max_screen_bytes
            .or_else(|| existing.map(Policy::max_screen_bytes))
            .unwrap_or(MAX_SCREEN_BYTES),
        max_concurrency: options
            .max_concurrency
            .or_else(|| existing.map(Policy::max_concurrency))
            .unwrap_or(MAX_CONCURRENCY),
        wildcard_confirmation: WildcardConfirmation::new(confirm_wildcard),
    })
    .map_err(hermes_error)
}

fn hermes_error(error: HermesError) -> CliError {
    CliError::Hermes { error }
}

fn agent_name(agent: HookAgentArg) -> &'static str {
    match agent {
        HookAgentArg::Hermes => "hermes",
        HookAgentArg::Claude => "claude",
        HookAgentArg::Codex => "codex",
    }
}

fn agent_label(agent: &AgentKind) -> &str {
    agent.as_wire()
}

fn render_install_human(result: &IntegrationInstallResult) -> String {
    if result.installed.is_empty() {
        return "no agent hooks installed\n".to_owned();
    }
    let mut output = String::new();
    for report in &result.installed {
        let _ = writeln!(
            output,
            "installed {} hook: {}",
            agent_label(&report.agent),
            report.hook_path
        );
        for path in &report.config_paths {
            let _ = writeln!(output, "  config: {path}");
        }
    }
    output
}

fn render_status_human(result: &IntegrationStatusResult) -> String {
    if result.agents.is_empty() {
        return "no agents selected\n".to_owned();
    }
    let mut output = String::from("AGENT       AVAILABLE  STATE         VERSION\n");
    for report in &result.agents {
        let _ = writeln!(
            output,
            "{:<11} {:<9} {:<13} {}",
            agent_label(&report.agent),
            report.available,
            state_label(report.state),
            version_label(report.installed_version, report.expected_version)
        );
        let _ = writeln!(output, "  hook: {}", report.expected_hook_path);
        for path in &report.managed_hook_paths {
            let _ = writeln!(output, "  managed: {path}");
        }
        if let Some(warning) = &report.warning {
            let _ = writeln!(output, "  warning: {warning}");
        }
    }
    output
}

fn state_label(state: IntegrationInstallState) -> &'static str {
    match state {
        IntegrationInstallState::NotInstalled => "not_installed",
        IntegrationInstallState::Current => "current",
        IntegrationInstallState::Outdated => "outdated",
    }
}

fn version_label(installed: Option<u32>, expected: u32) -> String {
    match installed {
        Some(version) => format!("{version}/{expected}"),
        None => format!("unknown/{expected}"),
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "the serialized lifecycle contract intentionally exposes independent findings"
)]
#[derive(Debug, Serialize)]
struct HermesResult {
    action: &'static str,
    target_kind: &'static str,
    target_label: String,
    installed: bool,
    enabled: bool,
    modified: bool,
    stale_stage: bool,
    stale_backup: bool,
    access_mode: Option<AccessMode>,
    allowed_host_count: Option<usize>,
    doctor: Option<doctor::Report>,
}

fn result(
    action: HermesAction,
    target: &crate::hermes_integration::target::ResolvedTarget,
    lifecycle: LifecycleState,
    policy: Option<&Policy>,
    doctor: Option<doctor::Report>,
) -> HermesResult {
    let (target_kind, target_label) = match target.invocation() {
        crate::hermes_integration::target::HermesInvocation::Profile(profile) => {
            ("profile", profile.as_str().to_owned())
        }
        crate::hermes_integration::target::HermesInvocation::CustomHome => {
            ("custom_home", "custom".to_owned())
        }
    };
    HermesResult {
        action: action.label(),
        target_kind,
        target_label,
        installed: lifecycle.installed,
        enabled: lifecycle.enabled,
        modified: lifecycle.modified,
        stale_stage: lifecycle.stale_stage,
        stale_backup: lifecycle.stale_backup,
        access_mode: policy.map(Policy::access_mode),
        allowed_host_count: policy.map(|policy| policy.allowed_hosts().len()),
        doctor,
    }
}

fn lifecycle_from_doctor(report: &doctor::Report) -> LifecycleState {
    let has_status = |code: &str, status: doctor::Status| {
        report
            .checks
            .iter()
            .any(|check| check.code == code && check.status == status)
    };
    LifecycleState {
        installed: has_status("plugin_ownership", doctor::Status::Pass),
        enabled: has_status("plugin_enabled", doctor::Status::Pass),
        modified: has_status("plugin_ownership", doctor::Status::Pass)
            && has_status("asset_integrity", doctor::Status::Fail),
        stale_stage: has_status("stale_stage", doctor::Status::Fail),
        stale_backup: has_status("stale_backup", doctor::Status::Fail),
    }
}

fn render_hermes_human(result: &HermesResult) -> String {
    let mut output = format!(
        "Hermes {}: {} {} (installed={}, enabled={}, modified={})\n",
        result.action,
        result.target_kind,
        result.target_label,
        result.installed,
        result.enabled,
        result.modified,
    );
    if let (Some(access_mode), Some(allowed_host_count)) =
        (result.access_mode, result.allowed_host_count)
    {
        let _ = writeln!(
            output,
            "policy: access_mode={access_mode:?}, allowed_hosts={allowed_host_count}"
        );
    }
    if let Some(report) = result.doctor.as_ref() {
        let _ = writeln!(output, "doctor: ok={}", report.ok);
        for check in report
            .checks
            .iter()
            .filter(|check| check.status != doctor::Status::Pass)
        {
            let _ = writeln!(
                output,
                "doctor: {}={:?}; recovery: {}",
                check.code, check.status, check.recovery_hint
            );
        }
    }
    output
}

#[cfg(test)]
mod status_tests {
    use super::{render_status_human, version_label};
    use protocol::{
        AgentKind, IntegrationAgentStatus, IntegrationInstallState, IntegrationStatusResult,
    };

    #[test]
    fn renders_status_table_with_paths_warnings_and_versions() {
        let result = IntegrationStatusResult {
            agents: vec![
                IntegrationAgentStatus {
                    agent: AgentKind::Claude,
                    available: true,
                    expected_hook_path: "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                    managed_hook_paths: vec![
                        "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                        "/home/u/.claude/hooks/pohunek-agent-notify.sh".to_owned(),
                    ],
                    installed_version: Some(4),
                    expected_version: 4,
                    state: IntegrationInstallState::Current,
                    warning: Some(
                        "notification hook version marker is missing, invalid, or outdated"
                            .to_owned(),
                    ),
                },
                IntegrationAgentStatus {
                    agent: AgentKind::Codex,
                    available: true,
                    expected_hook_path: "/home/u/.codex/pohunek-agent-state.sh".to_owned(),
                    managed_hook_paths: vec!["/home/u/.codex/pohunek-agent-state.sh".to_owned()],
                    installed_version: None,
                    expected_version: 4,
                    state: IntegrationInstallState::Outdated,
                    warning: Some(
                        "state hook version marker is missing, invalid, or outdated".to_owned(),
                    ),
                },
            ],
        };

        let output = render_status_human(&result);

        let rows: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("claude "))
            .collect();
        assert!(rows.len() == 1 && rows[0].contains("current") && rows[0].ends_with("4/4"));
        assert!(output.contains("  hook: /home/u/.claude/hooks/pohunek-agent-state.sh"));
        assert!(output.contains("  managed: /home/u/.claude/hooks/pohunek-agent-notify.sh"));
        assert!(output.contains(
            "  warning: notification hook version marker is missing, invalid, or outdated"
        ));
        let rows: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("codex "))
            .collect();
        assert!(rows.len() == 1 && rows[0].contains("outdated") && rows[0].ends_with("unknown/4"));
        assert!(output
            .contains("  warning: state hook version marker is missing, invalid, or outdated"));
    }

    #[test]
    fn renders_installed_and_expected_versions() {
        assert_eq!(version_label(Some(4), 5), "4/5");
        assert_eq!(version_label(None, 5), "unknown/5");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use clap::ValueEnum as _;
    use protocol::{AgentKind, IntegrationInstallReport, IntegrationInstallResult};

    use super::{
        agent_name, lifecycle_from_doctor, render_install_human, AccessModeArg, HookAgentArg,
    };
    use crate::hermes_integration::doctor;
    use crate::hermes_integration::policy::{
        AccessMode, Policy, PolicyInput, WildcardConfirmation, DEFAULT_REQUEST_TIMEOUT_MS,
        MAX_CONCURRENCY, MAX_OUTPUT_BYTES, MAX_SCREEN_BYTES, MAX_TIMEOUT_MS,
    };

    fn hermes_options() -> super::HermesOptions {
        super::HermesOptions {
            profile: None,
            home: None,
            hermes_bin: None,
            pohunek_bin: None,
            access_mode: None,
            allowed_hosts: vec![],
            tool_timeout_ms: None,
            request_timeout_ms: None,
            max_output_bytes: None,
            max_screen_bytes: None,
            max_concurrency: None,
            confirm_wildcard: false,
            confirm_modified: false,
        }
    }

    fn private_directory(path: &Path) {
        fs::create_dir_all(path).expect("create private directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("set private directory mode");
    }

    fn temporary_directory(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("pohunek-integration-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        private_directory(&path);
        path
    }

    fn doctor_report(statuses: &[(&'static str, doctor::Status)]) -> doctor::Report {
        let checks: Vec<doctor::Check> = statuses
            .iter()
            .copied()
            .map(|(code, status)| doctor::Check {
                code,
                status,
                recovery_hint: "test recovery",
            })
            .collect();
        let ok = checks
            .iter()
            .all(|check| check.status == doctor::Status::Pass);
        doctor::Report { ok, checks }
    }

    #[test]
    fn hook_agent_arg_maps_to_agent_kind() {
        assert_eq!(AgentKind::from(HookAgentArg::Claude), AgentKind::Claude);
        assert_eq!(AgentKind::from(HookAgentArg::Codex), AgentKind::Codex);
        assert_eq!(AgentKind::from(HookAgentArg::Hermes), AgentKind::Hermes);
        assert_eq!(agent_name(HookAgentArg::Hermes), "hermes");
        assert_eq!(
            AccessModeArg::ReadOnly
                .to_possible_value()
                .unwrap()
                .get_name(),
            "read_only"
        );
    }

    #[test]
    fn renders_install_reports_with_hook_and_config_paths() {
        let result = IntegrationInstallResult {
            installed: vec![
                IntegrationInstallReport {
                    agent: AgentKind::Claude,
                    hook_path: "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                    config_paths: vec!["/home/u/.claude/settings.json".to_owned()],
                },
                IntegrationInstallReport {
                    agent: AgentKind::Codex,
                    hook_path: "/home/u/.codex/pohunek-agent-state.sh".to_owned(),
                    config_paths: vec![
                        "/home/u/.codex/hooks.json".to_owned(),
                        "/home/u/.codex/config.toml".to_owned(),
                    ],
                },
            ],
        };

        let output = render_install_human(&result);

        assert!(output
            .contains("installed claude hook: /home/u/.claude/hooks/pohunek-agent-state.sh\n"));
        assert!(output.contains("  config: /home/u/.claude/settings.json\n"));
        assert!(output.contains("installed codex hook: /home/u/.codex/pohunek-agent-state.sh\n"));
        assert!(output.contains("  config: /home/u/.codex/hooks.json\n"));
        assert!(output.contains("  config: /home/u/.codex/config.toml\n"));
    }

    #[test]
    fn renders_empty_install_result() {
        let output = render_install_human(&IntegrationInstallResult { installed: vec![] });
        assert_eq!(output, "no agent hooks installed\n");
    }

    #[test]
    fn lifecycle_from_doctor_ignores_not_run_checks() {
        let report = doctor_report(&[
            ("plugin_ownership", doctor::Status::NotRun),
            ("asset_integrity", doctor::Status::NotRun),
            ("plugin_enabled", doctor::Status::NotRun),
            ("stale_stage", doctor::Status::NotRun),
            ("stale_backup", doctor::Status::NotRun),
        ]);

        let lifecycle = lifecycle_from_doctor(&report);

        assert!(!lifecycle.installed);
        assert!(!lifecycle.enabled);
        assert!(!lifecycle.modified);
        assert!(!lifecycle.stale_stage);
        assert!(!lifecycle.stale_backup);
    }

    #[test]
    fn lifecycle_from_doctor_reports_failed_stale_stage_check() {
        let report = doctor_report(&[
            ("stale_stage", doctor::Status::Fail),
            ("stale_backup", doctor::Status::NotRun),
        ]);

        let lifecycle = lifecycle_from_doctor(&report);

        assert!(lifecycle.stale_stage);
        assert!(!lifecycle.stale_backup);
    }

    #[test]
    fn lifecycle_from_doctor_maps_all_passed_checks() {
        let report = doctor_report(&[
            ("plugin_ownership", doctor::Status::Pass),
            ("asset_integrity", doctor::Status::Pass),
            ("plugin_enabled", doctor::Status::Pass),
            ("stale_stage", doctor::Status::Pass),
            ("stale_backup", doctor::Status::Pass),
        ]);

        let lifecycle = lifecycle_from_doctor(&report);

        assert!(lifecycle.installed);
        assert!(lifecycle.enabled);
        assert!(!lifecycle.modified);
        assert!(!lifecycle.stale_stage);
        assert!(!lifecycle.stale_backup);
    }

    #[test]
    fn lifecycle_from_doctor_requires_failed_asset_integrity_for_modified() {
        let not_run = doctor_report(&[
            ("plugin_ownership", doctor::Status::Pass),
            ("asset_integrity", doctor::Status::NotRun),
        ]);
        let failed = doctor_report(&[
            ("plugin_ownership", doctor::Status::Pass),
            ("asset_integrity", doctor::Status::Fail),
        ]);

        assert!(!lifecycle_from_doctor(&not_run).modified);
        assert!(lifecycle_from_doctor(&failed).modified);
    }

    #[test]
    fn renders_install_result_as_json_that_deserializes() {
        let result = IntegrationInstallResult {
            installed: vec![IntegrationInstallReport {
                agent: AgentKind::Claude,
                hook_path: "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                config_paths: vec!["/home/u/.claude/settings.json".to_owned()],
            }],
        };
        let doc = crate::commands::render_json(&result).expect("json doc");
        let parsed: IntegrationInstallResult = crate::commands::parse_json_ok(&doc);
        assert_eq!(parsed, result);
    }

    #[test]
    fn bounded_path_resolution_uses_only_absolute_entries() {
        let root = temporary_directory("path-resolution");
        let relative = root.join("relative");
        let absolute = root.join("absolute");
        private_directory(&relative);
        private_directory(&absolute);
        fs::write(relative.join("hermes"), b"not selected").expect("relative candidate");
        fs::write(absolute.join("hermes"), b"selected").expect("absolute candidate");
        let path =
            std::env::join_paths([Path::new("relative"), absolute.as_path()]).expect("test PATH");
        assert_eq!(
            super::resolve_hermes_from_path(&path).expect("absolute candidate"),
            absolute.join("hermes")
        );
        fs::remove_dir_all(root).expect("cleanup fixture");
    }

    #[test]
    fn install_requires_policy_inputs_and_new_wildcards_need_confirmation() {
        let base = hermes_options();
        assert_eq!(
            super::install_policy(&base)
                .expect_err("missing access mode")
                .to_protocol_error()
                .code,
            "hermes_invalid_policy"
        );
        let wildcard = super::HermesOptions {
            access_mode: Some(AccessModeArg::Manage),
            allowed_hosts: vec!["*".to_owned()],
            ..base
        };
        assert_eq!(
            super::install_policy(&wildcard)
                .expect_err("new wildcard needs confirmation")
                .to_protocol_error()
                .code,
            "hermes_wildcard_confirmation_required"
        );
    }

    #[test]
    fn install_policy_uses_explicit_bounds_or_ceiling_defaults() {
        let explicit = super::install_policy(&super::HermesOptions {
            access_mode: Some(AccessModeArg::Manage),
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: Some(MAX_TIMEOUT_MS / 2),
            request_timeout_ms: Some(MAX_TIMEOUT_MS / 4),
            max_output_bytes: Some(MAX_OUTPUT_BYTES / 2),
            max_screen_bytes: Some(MAX_SCREEN_BYTES / 2),
            max_concurrency: Some(MAX_CONCURRENCY / 2),
            ..hermes_options()
        })
        .expect("explicit policy bounds");
        assert_eq!(explicit.tool_timeout_ms(), MAX_TIMEOUT_MS / 2);
        assert_eq!(explicit.request_timeout_ms(), MAX_TIMEOUT_MS / 4);
        assert_eq!(explicit.max_output_bytes(), MAX_OUTPUT_BYTES / 2);
        assert_eq!(explicit.max_screen_bytes(), MAX_SCREEN_BYTES / 2);
        assert_eq!(explicit.max_concurrency(), MAX_CONCURRENCY / 2);

        let defaults = super::install_policy(&super::HermesOptions {
            access_mode: Some(AccessModeArg::Manage),
            allowed_hosts: vec!["local".to_owned()],
            ..hermes_options()
        })
        .expect("default policy bounds");
        assert_eq!(defaults.tool_timeout_ms(), MAX_TIMEOUT_MS);
        assert_eq!(defaults.request_timeout_ms(), DEFAULT_REQUEST_TIMEOUT_MS);
        assert_eq!(defaults.max_output_bytes(), MAX_OUTPUT_BYTES);
        assert_eq!(defaults.max_screen_bytes(), MAX_SCREEN_BYTES);
        assert_eq!(defaults.max_concurrency(), MAX_CONCURRENCY);
    }

    #[test]
    fn update_policy_inherits_or_replaces_bounds() {
        let existing = super::install_policy(&super::HermesOptions {
            access_mode: Some(AccessModeArg::Manage),
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: Some(MAX_TIMEOUT_MS / 2),
            request_timeout_ms: Some(MAX_TIMEOUT_MS / 4),
            max_output_bytes: Some(MAX_OUTPUT_BYTES / 2),
            max_screen_bytes: Some(MAX_SCREEN_BYTES / 2),
            max_concurrency: Some(MAX_CONCURRENCY / 2),
            ..hermes_options()
        })
        .expect("existing policy");

        let inherited =
            super::update_policy(&hermes_options(), &existing).expect("inherited policy bounds");
        assert_eq!(inherited.tool_timeout_ms(), existing.tool_timeout_ms());
        assert_eq!(
            inherited.request_timeout_ms(),
            existing.request_timeout_ms()
        );
        assert_eq!(inherited.max_output_bytes(), existing.max_output_bytes());
        assert_eq!(inherited.max_screen_bytes(), existing.max_screen_bytes());
        assert_eq!(inherited.max_concurrency(), existing.max_concurrency());

        let replaced = super::update_policy(
            &super::HermesOptions {
                tool_timeout_ms: Some(MAX_TIMEOUT_MS / 4),
                request_timeout_ms: Some(MAX_TIMEOUT_MS / 8),
                max_output_bytes: Some(MAX_OUTPUT_BYTES / 4),
                max_screen_bytes: Some(MAX_SCREEN_BYTES / 4),
                max_concurrency: Some(MAX_CONCURRENCY / 4),
                ..hermes_options()
            },
            &existing,
        )
        .expect("replacement policy bounds");
        assert_eq!(replaced.tool_timeout_ms(), MAX_TIMEOUT_MS / 4);
        assert_eq!(replaced.request_timeout_ms(), MAX_TIMEOUT_MS / 8);
        assert_eq!(replaced.max_output_bytes(), MAX_OUTPUT_BYTES / 4);
        assert_eq!(replaced.max_screen_bytes(), MAX_SCREEN_BYTES / 4);
        assert_eq!(replaced.max_concurrency(), MAX_CONCURRENCY / 4);
    }

    #[test]
    fn update_policy_refreshes_the_supported_protocol_range() {
        let versions = protocol::SUPPORTED_PROTOCOL_VERSIONS;
        let current_min = i32::try_from(versions.minimum().get()).expect("protocol minimum");
        let current_max = i32::try_from(versions.maximum().get()).expect("protocol maximum");
        let old_version = current_min.checked_sub(1).expect("older protocol version");
        let existing = Policy::new(PolicyInput {
            pohunek_cli: std::env::current_exe().expect("test executable"),
            protocol_min: old_version,
            protocol_max: old_version,
            access_mode: AccessMode::Manage,
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: MAX_TIMEOUT_MS / 2,
            request_timeout_ms: MAX_TIMEOUT_MS / 4,
            max_output_bytes: MAX_OUTPUT_BYTES / 2,
            max_screen_bytes: MAX_SCREEN_BYTES / 2,
            max_concurrency: MAX_CONCURRENCY / 2,
            wildcard_confirmation: WildcardConfirmation::new(false),
        })
        .expect("old installed policy");

        let updated = super::update_policy(&hermes_options(), &existing)
            .expect("policy with refreshed protocol range");

        assert_eq!(updated.protocol_min(), current_min);
        assert_eq!(updated.protocol_max(), current_max);
        assert_eq!(updated.tool_timeout_ms(), existing.tool_timeout_ms());
    }

    #[test]
    fn install_policy_returns_typed_error_for_out_of_range_bound() {
        let error = super::install_policy(&super::HermesOptions {
            access_mode: Some(AccessModeArg::Manage),
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: Some(MAX_TIMEOUT_MS + 1),
            ..hermes_options()
        })
        .expect_err("timeout above policy ceiling");

        assert_eq!(error.to_protocol_error().code, "hermes_invalid_policy");
    }

    #[test]
    fn every_hermes_action_requires_an_explicit_target_and_rejects_irrelevant_flags() {
        let no_target = hermes_options();
        for action in [
            super::HermesAction::Install,
            super::HermesAction::Status,
            super::HermesAction::Doctor,
            super::HermesAction::Update,
            super::HermesAction::Uninstall,
        ] {
            assert_eq!(
                no_target
                    .validate_for_action(action)
                    .expect_err("explicit target required")
                    .to_protocol_error()
                    .code,
                "integration_hermes_usage"
            );
        }
        let status_with_policy_flag = super::HermesOptions {
            profile: Some("default".to_owned()),
            access_mode: Some(AccessModeArg::Manage),
            ..no_target
        };
        assert_eq!(
            status_with_policy_flag
                .validate_for_action(super::HermesAction::Status)
                .expect_err("status cannot replace policy")
                .to_protocol_error()
                .code,
            "integration_hermes_usage"
        );

        for options in [
            super::HermesOptions {
                profile: Some("default".to_owned()),
                tool_timeout_ms: Some(MAX_TIMEOUT_MS),
                ..hermes_options()
            },
            super::HermesOptions {
                profile: Some("default".to_owned()),
                max_output_bytes: Some(MAX_OUTPUT_BYTES),
                ..hermes_options()
            },
            super::HermesOptions {
                profile: Some("default".to_owned()),
                max_screen_bytes: Some(MAX_SCREEN_BYTES),
                ..hermes_options()
            },
            super::HermesOptions {
                profile: Some("default".to_owned()),
                max_concurrency: Some(MAX_CONCURRENCY),
                ..hermes_options()
            },
        ] {
            assert!(options.is_explicit());
            for action in [
                super::HermesAction::Status,
                super::HermesAction::Doctor,
                super::HermesAction::Uninstall,
            ] {
                assert_eq!(
                    options
                        .validate_for_action(action)
                        .expect_err("policy bound is invalid for read/remove action")
                        .to_protocol_error()
                        .code,
                    "integration_hermes_usage"
                );
            }
        }
    }

    #[test]
    fn update_preserves_a_confirmed_wildcard_without_reconfirming_it() {
        let existing = super::install_policy(&super::HermesOptions {
            access_mode: Some(AccessModeArg::Manage),
            allowed_hosts: vec!["*".to_owned()],
            confirm_wildcard: true,
            ..hermes_options()
        })
        .expect("confirmed stored wildcard policy");
        let updated = super::update_policy(
            &super::HermesOptions {
                access_mode: Some(AccessModeArg::Full),
                ..hermes_options()
            },
            &existing,
        )
        .expect("stored wildcard is preserved");
        assert_eq!(
            updated.access_mode(),
            crate::hermes_integration::policy::AccessMode::Full
        );
        assert_eq!(updated.allowed_hosts().collect::<Vec<_>>(), ["*"]);
    }

    #[test]
    fn hermes_output_is_json_enveloped_and_never_includes_a_custom_home_path() {
        let output = super::HermesResult {
            action: "status",
            target_kind: "custom_home",
            target_label: "custom".to_owned(),
            installed: true,
            enabled: true,
            modified: false,
            stale_stage: false,
            stale_backup: false,
            access_mode: None,
            allowed_host_count: None,
            doctor: None,
        };
        let human = super::render_hermes_human(&output);
        assert!(human.contains("custom_home custom"));
        assert!(!human.contains("/private/"));
        let json = crate::commands::render_json(&output).expect("JSON result");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse JSON result");
        assert_eq!(value["ok"]["action"], "status");
        assert!(!json.contains("/private/"));
    }
}
