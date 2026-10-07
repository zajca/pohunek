//! `pohunek integration` lifecycle commands.
//!
//! Codex and Claude hook install, uninstall, status, and doctor are daemon RPCs
//! (install and uninstall always target the local daemon; status and doctor
//! follow `--host`). Hermes is an owner-local plugin lifecycle and deliberately
//! never contacts the daemon.

// Rust guideline compliant 2026-10-05

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use protocol::{
    method, ErrorClass, IntegrationDoctorParams, IntegrationDoctorResult,
    IntegrationFindingSeverity, IntegrationHome, IntegrationHomeFailure, IntegrationInstallParams,
    IntegrationInstallResult, IntegrationInstallState, IntegrationSelector,
    IntegrationStatusParams, IntegrationStatusResult, IntegrationUninstallParams,
    IntegrationUninstallResult, IntegrationUninstallState, ProtocolError, RuntimeId, RuntimeRef,
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
///
/// The built-in names keep their meaning; any other value is a runtime id the
/// daemon resolves to the integration handler its definition names, so a
/// runtime id that no installed runtime backs is refused by the daemon with its
/// typed error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookAgentArg {
    /// Install or manage the Hermes operator plugin locally.
    Hermes,
    /// Install the Claude Code hook.
    Claude,
    /// Install the Codex hook.
    Codex,
    /// A runtime the daemon resolves through its definition's handler.
    Runtime(RuntimeId),
}

/// Wire names of the built-in agents, offered to help and completion.
const BUILTIN_AGENT_NAMES: [&str; 3] = ["claude", "codex", "hermes"];

impl std::str::FromStr for HookAgentArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "hermes" => Ok(Self::Hermes),
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => RuntimeId::parse(other)
                .map(Self::Runtime)
                .map_err(|error| format!("`{other}` is not a runtime id ({error})")),
        }
    }
}

/// Clap parser of [`HookAgentArg`] that offers the built-in names as possible
/// values without restricting the argument to them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HookAgentParser;

impl clap::builder::TypedValueParser for HookAgentParser {
    type Value = HookAgentArg;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let text = value.to_str().ok_or_else(|| {
            cmd.clone().error(
                clap::error::ErrorKind::InvalidUtf8,
                "the agent is not UTF-8",
            )
        })?;
        text.parse().map_err(|message: String| {
            cmd.clone()
                .error(clap::error::ErrorKind::ValueValidation, message)
        })
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        Some(Box::new(
            BUILTIN_AGENT_NAMES
                .into_iter()
                .map(clap::builder::PossibleValue::new),
        ))
    }
}

/// The parser of `--agent` values.
pub(crate) fn hook_agent_parser() -> HookAgentParser {
    HookAgentParser
}

impl From<HookAgentArg> for RuntimeRef {
    fn from(value: HookAgentArg) -> Self {
        match value {
            HookAgentArg::Claude => RuntimeRef::claude(),
            HookAgentArg::Codex => RuntimeRef::codex(),
            HookAgentArg::Hermes => RuntimeRef::hermes(),
            HookAgentArg::Runtime(id) => RuntimeRef::from_wire(id.as_str()),
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

/// Which config home of a Codex or Claude runtime a command acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HomeSelectionArgs {
    /// Host profile whose config home is selected.
    pub(crate) profile: Option<String>,
    /// Every distinct config home of the selected runtime.
    pub(crate) all_profiles: bool,
}

impl HomeSelectionArgs {
    /// Whether the invocation names a home other than the runtime's own.
    #[must_use]
    pub(crate) fn is_selected(&self) -> bool {
        self.profile.is_some() || self.all_profiles
    }

    /// Refuses a home selection for the Hermes plugin, which lives in a Hermes
    /// home chosen with `--hermes-profile` or `--hermes-home`.
    ///
    /// # Errors
    ///
    /// A typed configuration error when a selector is present.
    pub(crate) fn refuse_for_hermes(&self) -> Result<(), CliError> {
        if self.is_selected() {
            return Err(CliError::Protocol(ProtocolError::new(
                ErrorClass::Configuration,
                "integration_profile_not_for_hermes",
                "`--profile` and `--all-profiles` select Codex or Claude config homes, not a Hermes home"
                    .to_owned(),
                Some("use `--hermes-profile` or `--hermes-home` for Hermes".to_owned()),
            )));
        }
        Ok(())
    }
}

/// `integration_home_selectors_unsupported`: the daemon predates `--profile` and
/// `--all-profiles`, so it would act on the runtime's own home.
fn selectors_unsupported() -> CliError {
    CliError::Protocol(ProtocolError::new(
        ErrorClass::Configuration,
        "integration_home_selectors_unsupported",
        "the daemon does not support `--profile` and `--all-profiles`; it would act on the default config home instead"
            .to_owned(),
        Some("update and restart the pohunek daemon to the release of this CLI".to_owned()),
    ))
}

/// The unsupported-selector error for a daemon that rejected the selectors as
/// unknown fields; any other error is unchanged.
fn unsupported_if_rejected(error: CliError, home: &HomeSelectionArgs) -> CliError {
    match error {
        CliError::Protocol(inner) if home.is_selected() && inner.code == "bad_request" => {
            selectors_unsupported()
        }
        other => other,
    }
}

/// Proves, with a read-only request on `client`, that the daemon honors the
/// home selectors before a mutation that carries them is sent.
///
/// A daemon that predates the selectors ignores them on install and returns no
/// `home_selectors` marker (or rejects them as unknown fields), and a request
/// it ignored the selectors of would change the wrong home. Nothing is sent
/// when no selector is set.
async fn require_selector_support(
    client: &mut Client,
    agent: Option<RuntimeRef>,
    home: &HomeSelectionArgs,
) -> Result<(), CliError> {
    if !home.is_selected() {
        return Ok(());
    }
    let probe = IntegrationStatusParams {
        agent,
        profile: home.profile.clone(),
        all_profiles: home.all_profiles,
    };
    match client.call::<method::IntegrationStatus>(probe).await {
        Ok(result) if result.home_selectors => Ok(()),
        Ok(_) => Err(selectors_unsupported()),
        Err(CliError::Protocol(error)) if error.code == "bad_request" => {
            Err(selectors_unsupported())
        }
        Err(error) => Err(error),
    }
}

/// How a result names the config home it describes.
fn home_label(home: &IntegrationHome) -> String {
    let mut parts = Vec::new();
    if home.bare {
        parts.push("default".to_owned());
    }
    parts.extend(
        home.profiles
            .iter()
            .map(|profile| format!("profile {profile}")),
    );
    parts.join(", ")
}

/// Run the original daemon-backed Codex or Claude hook installation.
///
/// Returns whether every selected home was installed.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request, or
/// returns an unexpected payload.
pub(crate) async fn run_install(
    paths: &Paths,
    agent: Option<HookAgentArg>,
    home: &HomeSelectionArgs,
    json: bool,
) -> Result<bool, CliError> {
    let params = IntegrationInstallParams {
        agent: agent.map(Into::into),
        profile: home.profile.clone(),
        all_profiles: home.all_profiles,
    };
    // Installing hooks is inherently a *local* daemon operation: it writes into
    // this machine's agent config dirs. Always use the local transport regardless
    // of any `--host` flag.
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    require_selector_support(&mut client, params.agent.clone(), home).await?;
    let result: IntegrationInstallResult =
        client.call::<method::IntegrationInstall>(params).await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_install_human(&result));
    }
    Ok(result.failed.is_empty())
}

/// Run the read-only Codex and Claude integration status RPC.
///
/// # Errors
///
/// Returns [`CliError`] if the effective host daemon is unreachable or rejects the call.
pub(crate) async fn run_status(
    host: &str,
    paths: &Paths,
    agent: Option<HookAgentArg>,
    home: &HomeSelectionArgs,
    json: bool,
) -> Result<(), CliError> {
    let params = IntegrationStatusParams {
        agent: agent.map(Into::into),
        profile: home.profile.clone(),
        all_profiles: home.all_profiles,
    };
    let mut client = Client::connect(host, paths).await?;
    let result: IntegrationStatusResult = client
        .call::<method::IntegrationStatus>(params)
        .await
        .map_err(|error| unsupported_if_rejected(error, home))?;
    if home.is_selected() && !result.home_selectors {
        return Err(selectors_unsupported());
    }

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_status_human(host, &result));
    }
    Ok(())
}

/// Run the daemon-backed Codex and Claude hook removal.
///
/// Returns whether every selected home was removed from.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request, or
/// returns an unexpected payload.
pub(crate) async fn run_uninstall(
    paths: &Paths,
    agent: HookAgentArg,
    home: &HomeSelectionArgs,
    json: bool,
) -> Result<bool, CliError> {
    let params = IntegrationUninstallParams {
        agent: agent.into(),
        profile: home.profile.clone(),
        all_profiles: home.all_profiles,
    };
    // Removal edits this machine's agent config dirs, so it always uses the
    // local transport regardless of any `--host` flag.
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    require_selector_support(&mut client, Some(params.agent.clone()), home).await?;
    let result: IntegrationUninstallResult =
        client.call::<method::IntegrationUninstall>(params).await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_uninstall_human(&result));
    }
    Ok(result.failed.is_empty())
}

/// Run the read-only Codex and Claude integration doctor RPC.
///
/// Returns whether every diagnosed agent is free of error findings.
///
/// # Errors
///
/// Returns [`CliError`] if the effective host daemon is unreachable or rejects the call.
pub(crate) async fn run_doctor(
    host: &str,
    paths: &Paths,
    agent: Option<HookAgentArg>,
    home: &HomeSelectionArgs,
    json: bool,
) -> Result<bool, CliError> {
    let params = IntegrationDoctorParams {
        agent: agent.map(Into::into),
        profile: home.profile.clone(),
        all_profiles: home.all_profiles,
    };
    let mut client = Client::connect(host, paths).await?;
    let result: IntegrationDoctorResult = client
        .call::<method::IntegrationDoctor>(params)
        .await
        .map_err(|error| unsupported_if_rejected(error, home))?;
    if home.is_selected() && !result.home_selectors {
        return Err(selectors_unsupported());
    }

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_doctor_human(&result));
    }
    Ok(result.ok)
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
            let (lifecycle, policy) = lifecycle::update(
                &mut runner,
                &target,
                &policy_path,
                options.confirm_modified,
                |existing| update_policy(options, existing),
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
    let agent = agent.map_or_else(|| "none".to_owned(), agent_name);
    CliError::Protocol(ProtocolError::new(
        ErrorClass::Configuration,
        "integration_action_unsupported",
        format!("integration lifecycle action is supported only for Hermes, not {agent}"),
        Some("pass `--agent hermes` for update".to_owned()),
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
        other => CliError::Paths(other),
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
    .map_err(hermes_error)
}

fn update_policy(options: &HermesOptions, existing: &Policy) -> Result<Policy, HermesError> {
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
) -> Result<Policy, HermesError> {
    let pohunek_cli = match (options.pohunek_bin.clone(), existing) {
        (Some(path), _) => path,
        (None, Some(policy)) => policy.pohunek_cli().to_owned(),
        (None, None) => env::current_exe().map_err(HermesError::from)?,
    };
    // Policies always use the range of the shape this binary prints, exactly the
    // current version, so updates repair drift that
    // `assets/pohunek/cli.py::_validate_envelope` rejects as `pohunek_cli_incompatible`.
    let versions = protocol::CURRENT_PROTOCOL_VERSIONS;
    let protocol_min =
        i32::try_from(versions.minimum().get()).map_err(|_error| HermesError::InvalidPolicy)?;
    let protocol_max =
        i32::try_from(versions.maximum().get()).map_err(|_error| HermesError::InvalidPolicy)?;
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
}

fn hermes_error(error: HermesError) -> CliError {
    CliError::Hermes { error }
}

fn agent_name(agent: HookAgentArg) -> String {
    RuntimeRef::from(agent).as_wire().to_owned()
}

fn agent_label(agent: &RuntimeRef) -> &str {
    agent.as_wire()
}

/// The lines that describe the homes whose transaction failed.
fn render_failures(output: &mut String, failed: &[IntegrationHomeFailure]) {
    for failure in failed {
        let _ = writeln!(
            output,
            "failed {} ({}): {}: {}",
            agent_label(&failure.agent),
            home_label(&failure.home),
            failure.error.code,
            failure.error.msg
        );
        if let Some(recover) = &failure.error.recover {
            let _ = writeln!(output, "  fix: {recover}");
        }
    }
}

fn render_install_human(result: &IntegrationInstallResult) -> String {
    if result.installed.is_empty() && result.failed.is_empty() {
        return "no agent hooks installed\n".to_owned();
    }
    let mut output = String::new();
    for report in &result.installed {
        let home = report
            .home
            .as_ref()
            .map_or_else(String::new, |home| format!(" ({})", home_label(home)));
        let _ = writeln!(
            output,
            "installed {} hook{home}: {}",
            agent_label(&report.agent),
            report.hook_path
        );
        for path in &report.config_paths {
            let _ = writeln!(output, "  config: {path}");
        }
        for leftover in &report.cleanup_incomplete {
            let _ = writeln!(output, "  cleanup incomplete: {leftover}");
        }
    }
    render_failures(&mut output, &result.failed);
    output
}

fn render_uninstall_human(result: &IntegrationUninstallResult) -> String {
    let mut output = String::new();
    for report in &result.uninstalled {
        let agent = match &report.home {
            Some(home) => format!("{} ({})", agent_label(&report.agent), home_label(home)),
            None => agent_label(&report.agent).to_owned(),
        };
        match report.state {
            IntegrationUninstallState::NotInstalled => {
                let _ = writeln!(
                    output,
                    "{agent}: no managed hooks installed, nothing removed"
                );
            }
            IntegrationUninstallState::Removed => {
                let _ = writeln!(output, "{agent}: removed managed hooks");
            }
        }
        for path in &report.removed_paths {
            let _ = writeln!(output, "  removed: {path}");
        }
        for path in &report.updated_paths {
            let _ = writeln!(output, "  updated registration: {path}");
        }
        for path in &report.preserved_paths {
            let _ = writeln!(output, "  preserved (not installer-owned): {path}");
        }
        for leftover in &report.cleanup_incomplete {
            let _ = writeln!(output, "  cleanup incomplete: {leftover}");
        }
    }
    render_failures(&mut output, &result.failed);
    output
}

fn render_doctor_human(result: &IntegrationDoctorResult) -> String {
    let mut output = String::new();
    for agent in &result.agents {
        let verdict = if agent.ok { "ok" } else { "needs attention" };
        let home = agent
            .home
            .as_ref()
            .map_or_else(String::new, |home| format!(" ({})", home_label(home)));
        let _ = writeln!(output, "{}{home}: {verdict}", agent_label(&agent.agent));
        for finding in &agent.findings {
            let severity = match finding.severity {
                IntegrationFindingSeverity::Info => "info",
                IntegrationFindingSeverity::Error => "error",
            };
            let code = serde_json::to_value(finding.code)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            let _ = writeln!(output, "  [{severity}] {code}: {}", finding.summary);
            if let Some(remediation) = &finding.remediation {
                let _ = writeln!(output, "    fix: {remediation}");
            }
        }
    }
    output
}

fn render_status_human(host: &str, result: &IntegrationStatusResult) -> String {
    if result.agents.is_empty() {
        return "no agents selected\n".to_owned();
    }
    let mut output = String::from("AGENT       AVAILABLE  STATE          INSTALLED  EXPECTED\n");
    for report in &result.agents {
        let _ = writeln!(
            output,
            "{:<11} {:<9} {:<14} {:<10} {}",
            agent_label(&report.agent),
            report.available,
            state_label(report.state),
            installed_version_label(report.installed_version),
            report.expected_version,
        );
        if let Some(home) = &report.home {
            let _ = writeln!(output, "  home: {}", home_label(home));
        }
        for path in &report.expected_asset_paths {
            let _ = writeln!(output, "  expected asset: {path}");
        }
        for path in &report.present_asset_paths {
            let _ = writeln!(output, "  present asset: {path}");
        }
        for path in &report.registration_paths {
            let _ = writeln!(output, "  registration: {path}");
        }
        for warning in &report.warnings {
            let warning = qualify_status_warning(host, warning);
            let _ = writeln!(output, "  warning: {warning}");
        }
        match report.recovery {
            protocol::IntegrationRecovery::None => {}
            protocol::IntegrationRecovery::Reinstall => {
                let agent = recovery_selector(report);
                if host.is_empty() || host == LOCAL_HOST {
                    let _ = writeln!(
                        output,
                        "  hint: run `pohunek integration install --agent {agent}` to repair"
                    );
                } else {
                    let _ = writeln!(
                        output,
                        "  hint: on daemon host `{host}`, run `pohunek integration install --agent {agent}` directly to repair"
                    );
                }
            }
            protocol::IntegrationRecovery::RepairConfiguration => {
                if host.is_empty() || host == LOCAL_HOST {
                    let _ = writeln!(
                        output,
                        "  hint: inspect and repair the reported provider configuration before reinstalling"
                    );
                } else {
                    let _ = writeln!(
                        output,
                        "  hint: inspect and repair the reported provider configuration directly on daemon host `{host}` before reinstalling there"
                    );
                }
            }
        }
    }
    output
}

/// The `--agent` value of a recovery command, followed by the `--profile` the
/// daemon verified reaches the home the report describes.
fn recovery_selector(report: &protocol::IntegrationAgentStatus) -> String {
    let agent = agent_label(&report.agent);
    match report.home.as_ref().and_then(|home| home.selector.as_ref()) {
        Some(IntegrationSelector::Profile { name }) => format!("{agent} --profile {name}"),
        Some(IntegrationSelector::Default) | None => agent.to_owned(),
    }
}

fn qualify_status_warning(host: &str, warning: &str) -> String {
    if host.is_empty() || host == LOCAL_HOST {
        return warning.to_owned();
    }
    let qualified = warning.replacen(
        "run `pohunek integration install",
        &format!("on daemon host `{host}`, run `pohunek integration install"),
        1,
    );
    if qualified != warning {
        return qualified;
    }
    warning.replacen(
        "running `pohunek integration install",
        &format!("running on daemon host `{host}`: `pohunek integration install"),
        1,
    )
}

fn state_label(state: IntegrationInstallState) -> &'static str {
    match state {
        IntegrationInstallState::NotInstalled => "not_installed",
        IntegrationInstallState::Current => "current",
        IntegrationInstallState::Outdated => "outdated",
    }
}

fn installed_version_label(installed: Option<u32>) -> String {
    installed.map_or_else(|| "none".to_owned(), |version| version.to_string())
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
    outdated: bool,
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
        outdated: lifecycle.outdated,
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
        outdated: has_status("plugin_ownership", doctor::Status::Pass)
            && has_status("asset_current", doctor::Status::Fail),
        stale_stage: has_status("stale_stage", doctor::Status::Fail),
        stale_backup: has_status("stale_backup", doctor::Status::Fail),
    }
}

fn render_hermes_human(result: &HermesResult) -> String {
    let mut output = format!(
        "Hermes {}: {} {} (installed={}, enabled={}, modified={}, outdated={})\n",
        result.action,
        result.target_kind,
        result.target_label,
        result.installed,
        result.enabled,
        result.modified,
        result.outdated,
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
    use super::render_status_human;
    use crate::target::LOCAL_HOST;
    use protocol::{
        IntegrationAgentStatus, IntegrationInstallState, IntegrationRecovery,
        IntegrationStatusResult, RuntimeRef,
    };

    #[test]
    fn renders_status_table_with_paths_warnings_and_versions() {
        let result = IntegrationStatusResult {
            home_selectors: false,
            agents: vec![
                IntegrationAgentStatus {
                    home: None,
                    agent: RuntimeRef::claude(),
                    available: true,
                    expected_asset_paths: vec![
                        "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                        "/home/u/.claude/hooks/pohunek-agent-notify.sh".to_owned(),
                    ],
                    present_asset_paths: vec![
                        "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                        "/home/u/.claude/hooks/pohunek-agent-notify.sh".to_owned(),
                    ],
                    registration_paths: vec!["/home/u/.claude/settings.json".to_owned()],
                    installed_version: Some(4),
                    expected_version: 4,
                    state: IntegrationInstallState::Current,
                    recovery: IntegrationRecovery::None,
                    warnings: Vec::new(),
                },
                IntegrationAgentStatus {
                    home: None,
                    agent: RuntimeRef::codex(),
                    available: true,
                    expected_asset_paths: vec![
                        "/home/u/.codex/pohunek-agent-state.sh".to_owned(),
                        "/home/u/.codex/pohunek-agent-notify.sh".to_owned(),
                    ],
                    present_asset_paths: vec!["/home/u/.codex/pohunek-agent-state.sh".to_owned()],
                    registration_paths: vec![
                        "/home/u/.codex/hooks.json".to_owned(),
                        "/home/u/.codex/config.toml".to_owned(),
                    ],
                    installed_version: None,
                    expected_version: 4,
                    state: IntegrationInstallState::Outdated,
                    recovery: IntegrationRecovery::Reinstall,
                    warnings: vec!["managed notification hook is missing".to_owned()],
                },
            ],
        };

        let output = render_status_human(LOCAL_HOST, &result);

        let rows: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("claude "))
            .collect();
        assert!(rows.len() == 1 && rows[0].contains("current") && rows[0].ends_with('4'));
        assert!(output.contains("  expected asset: /home/u/.claude/hooks/pohunek-agent-state.sh"));
        assert!(output.contains("  present asset: /home/u/.claude/hooks/pohunek-agent-notify.sh"));
        assert!(output.contains("  registration: /home/u/.claude/settings.json"));
        let rows: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("codex "))
            .collect();
        assert!(rows.len() == 1 && rows[0].contains("outdated") && rows[0].contains("none"));
        assert!(output.contains("  warning: managed notification hook is missing"));
        assert!(
            output.contains("  hint: run `pohunek integration install --agent codex` to repair")
        );
    }

    #[test]
    fn configuration_recovery_never_recommends_reinstall() {
        let result = IntegrationStatusResult {
            home_selectors: false,
            agents: vec![IntegrationAgentStatus {
                home: None,
                agent: RuntimeRef::codex(),
                available: true,
                expected_asset_paths: Vec::new(),
                present_asset_paths: Vec::new(),
                registration_paths: vec!["/home/u/.codex/config.toml".to_owned()],
                installed_version: None,
                expected_version: 4,
                state: IntegrationInstallState::Outdated,
                recovery: IntegrationRecovery::RepairConfiguration,
                warnings: vec!["Codex config.toml is malformed".to_owned()],
            }],
        };

        let output = render_status_human(LOCAL_HOST, &result);

        assert!(output.contains(
            "  hint: inspect and repair the reported provider configuration before reinstalling"
        ));
        assert!(!output.contains("pohunek integration install"));
    }

    #[test]
    fn remote_status_qualifies_install_commands_with_the_daemon_host() {
        let result = IntegrationStatusResult { home_selectors: false,
            agents: vec![IntegrationAgentStatus { home: None,
                agent: RuntimeRef::codex(),
                available: true,
                expected_asset_paths: Vec::new(),
                present_asset_paths: Vec::new(),
                registration_paths: Vec::new(),
                installed_version: Some(3),
                expected_version: 4,
                state: IntegrationInstallState::Outdated,
                recovery: IntegrationRecovery::Reinstall,
                warnings: vec![
                    "managed state hook permissions drifted; run `pohunek integration install --agent codex` to restore them".to_owned(),
                    "managed asset parent is unsafe; repair its permissions before running `pohunek integration install --agent codex`".to_owned(),
                ],
            }],
        };

        let output = render_status_human("buildbox", &result);

        assert!(output.contains(
            "warning: managed state hook permissions drifted; on daemon host `buildbox`, run `pohunek integration install --agent codex` to restore them"
        ));
        assert!(output.contains(
            "hint: on daemon host `buildbox`, run `pohunek integration install --agent codex` directly to repair"
        ));
        assert!(output.contains(
            "warning: managed asset parent is unsafe; repair its permissions before running on daemon host `buildbox`: `pohunek integration install --agent codex`"
        ));
        assert!(!output.contains("pohunek --host buildbox integration install"));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use protocol::{
        ErrorClass, IntegrationHome, IntegrationHomeFailure, IntegrationInstallReport,
        IntegrationInstallResult, IntegrationUninstallReport, IntegrationUninstallResult,
        IntegrationUninstallState, ProtocolError, RuntimeRef,
    };

    use super::{
        lifecycle_from_doctor, render_doctor_human, render_install_human, render_uninstall_human,
        AccessModeArg,
    };
    use crate::hermes_integration::doctor;
    use crate::hermes_integration::lifecycle::LifecycleState;
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

    /// Returns a private fixture directory guard and a private child directory of
    /// it; the guard removes everything when it drops, also when a test fails.
    fn temporary_directory(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let guard = pohunek_test_support::tempdir().expect("create fixture directory");
        let path = guard.path().join(tag);
        private_directory(&path);
        (guard, path)
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
    fn renders_install_reports_with_hook_and_config_paths() {
        let result = IntegrationInstallResult {
            failed: Vec::new(),
            installed: vec![
                IntegrationInstallReport {
                    home: None,
                    agent: RuntimeRef::claude(),
                    hook_path: "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                    config_paths: vec!["/home/u/.claude/settings.json".to_owned()],
                    cleanup_incomplete: vec![],
                },
                IntegrationInstallReport {
                    home: None,
                    agent: RuntimeRef::codex(),
                    hook_path: "/home/u/.codex/pohunek-agent-state.sh".to_owned(),
                    config_paths: vec![
                        "/home/u/.codex/hooks.json".to_owned(),
                        "/home/u/.codex/config.toml".to_owned(),
                    ],
                    cleanup_incomplete: vec![],
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
    fn renders_the_home_of_each_report_and_the_failed_homes() {
        let work = IntegrationHome {
            selector: None,
            profiles: vec!["work".to_owned()],
            bare: false,
        };
        let shared = IntegrationHome {
            selector: None,
            profiles: vec!["alt".to_owned(), "work".to_owned()],
            bare: true,
        };
        let result = IntegrationInstallResult {
            installed: vec![IntegrationInstallReport {
                agent: RuntimeRef::claude(),
                hook_path: "/p/work/hooks/pohunek-agent-state.sh".to_owned(),
                config_paths: vec![],
                cleanup_incomplete: vec![],
                home: Some(work.clone()),
            }],
            failed: vec![IntegrationHomeFailure {
                agent: RuntimeRef::claude(),
                home: shared.clone(),
                error: ProtocolError::new(
                    ErrorClass::Runtime,
                    "integration_settings_invalid",
                    "settings.json is not valid JSON",
                    Some("repair the file".to_owned()),
                ),
            }],
        };

        let output = render_install_human(&result);

        assert!(output.contains("installed claude hook (profile work): /p/work/hooks/"));
        assert!(output.contains(
            "failed claude (default, profile alt, profile work): integration_settings_invalid: settings.json is not valid JSON\n  fix: repair the file\n"
        ));
        let removal = IntegrationUninstallResult {
            uninstalled: vec![IntegrationUninstallReport {
                agent: RuntimeRef::claude(),
                state: IntegrationUninstallState::NotInstalled,
                removed_paths: vec![],
                updated_paths: vec![],
                preserved_paths: vec![],
                cleanup_incomplete: vec![],
                home: Some(work),
            }],
            failed: vec![],
        };
        assert!(render_uninstall_human(&removal)
            .starts_with("claude (profile work): no managed hooks installed"));
        let only_failures = IntegrationInstallResult {
            installed: vec![],
            failed: result.failed,
        };
        assert!(
            render_install_human(&only_failures)
                .starts_with("failed claude (default, profile alt, profile work): "),
            "a request that only failed reports its failures, not an empty install"
        );
    }

    #[test]
    fn lifecycle_from_doctor_derives_state_only_from_decisive_checks() {
        use doctor::Status::{Fail, NotRun, Pass};

        // A named case: the doctor check statuses and the lifecycle they imply.
        type Case<'a> = (
            &'a str,
            &'a [(&'static str, doctor::Status)],
            LifecycleState,
        );

        let state = |installed, enabled, modified, stale_stage, stale_backup| LifecycleState {
            installed,
            enabled,
            modified,
            stale_stage,
            stale_backup,
        };
        let cases: [Case<'_>; 5] = [
            (
                "checks that did not run report nothing",
                &[
                    ("plugin_ownership", NotRun),
                    ("asset_integrity", NotRun),
                    ("plugin_enabled", NotRun),
                    ("stale_stage", NotRun),
                    ("stale_backup", NotRun),
                ],
                state(false, false, false, false, false),
            ),
            (
                "a failed stale-stage check reports only the stale stage",
                &[("stale_stage", Fail), ("stale_backup", NotRun)],
                state(false, false, false, true, false),
            ),
            (
                "passed checks report an installed, enabled, clean plugin",
                &[
                    ("plugin_ownership", Pass),
                    ("asset_integrity", Pass),
                    ("plugin_enabled", Pass),
                    ("stale_stage", Pass),
                    ("stale_backup", Pass),
                ],
                state(true, true, false, false, false),
            ),
            (
                "an asset integrity check that did not run is not a modification",
                &[("plugin_ownership", Pass), ("asset_integrity", NotRun)],
                state(true, false, false, false, false),
            ),
            (
                "a failed asset integrity check of an owned plugin is a modification",
                &[("plugin_ownership", Pass), ("asset_integrity", Fail)],
                state(true, false, true, false, false),
            ),
        ];
        for (case, statuses, expected) in cases {
            assert_eq!(
                lifecycle_from_doctor(&doctor_report(statuses)),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn bounded_path_resolution_uses_only_absolute_entries() {
        let (_guard, root) = temporary_directory("path-resolution");
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
        let versions = protocol::CURRENT_PROTOCOL_VERSIONS;
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
            outdated: false,
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

    #[test]
    fn doctor_human_output_marks_severity_code_and_fix() {
        use protocol::{
            IntegrationAgentDoctor, IntegrationAgentStatus, IntegrationDoctorResult,
            IntegrationFinding, IntegrationFindingCode, IntegrationFindingSeverity,
            IntegrationRecovery,
        };

        let status = IntegrationAgentStatus {
            home: None,
            agent: RuntimeRef::codex(),
            available: true,
            expected_asset_paths: vec![],
            present_asset_paths: vec![],
            registration_paths: vec![],
            installed_version: None,
            expected_version: protocol::EXPECTED_INTEGRATION_VERSION,
            state: protocol::IntegrationInstallState::Outdated,
            recovery: IntegrationRecovery::Reinstall,
            warnings: vec![],
        };
        let result = IntegrationDoctorResult {
            home_selectors: false,
            ok: false,
            agents: vec![IntegrationAgentDoctor {
                home: None,
                agent: RuntimeRef::codex(),
                ok: false,
                status: Some(status),
                findings: vec![
                    IntegrationFinding {
                        code: IntegrationFindingCode::HookRuntimeMacosShim,
                        severity: IntegrationFindingSeverity::Error,
                        summary: "stub only".to_owned(),
                        remediation: Some("run `xcode-select --install`".to_owned()),
                    },
                    IntegrationFinding {
                        code: IntegrationFindingCode::AgentNotInstalled,
                        severity: IntegrationFindingSeverity::Info,
                        summary: "absent".to_owned(),
                        remediation: None,
                    },
                ],
            }],
        };

        assert_eq!(
            render_doctor_human(&result),
            "codex: needs attention\n  [error] hook_runtime_macos_shim: stub only\n    fix: run `xcode-select --install`\n  [info] agent_not_installed: absent\n"
        );
    }
}
