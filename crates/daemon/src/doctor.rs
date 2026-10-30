//! Daemon-local environment health checks.
//!
//! The host-probe logic is shared with the CLI `doctor` command via the
//! `hostcheck` crate; this module only selects which checks to run on the host
//! that owns the agent runtime and assembles them into a [`DoctorReport`].

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hostcheck::{OverlayChecks, StandardCheckInputs, Supervision, WorkerCandidate, WorkerSource};
use protocol::{
    DoctorCheck, DoctorReport, DoctorStatus, IntegrationDoctorParams, IntegrationFindingCode,
    IntegrationInstallState, QuarantineReason, RuntimeId, MAX_CONTROL_LINE_BYTES,
};

use crate::agent::host::VersionRefusal;
use crate::catalog_anchor::{AnchorFault, CatalogTrust};
use crate::governance::{HostGovernanceDiagnostic, HostGovernanceService};
use crate::session::{ActiveSupervision, RejectedLinesState};
use crate::Paths;

/// Name of the check that reports the metadata-store lines no loader accepts.
const METADATA_STORE_REJECTED_LINES_CHECK: &str = "metadata_store_rejected_lines";

/// The bounded daemon doctor task did not produce a report.
///
/// This deliberately omits Tokio's join details because a task panic payload
/// must not become part of a public diagnostic surface.
#[derive(Debug, thiserror::Error)]
#[error("daemon doctor host-check task failed")]
pub struct DoctorReportError;

/// Build a daemon-local doctor report on the host that owns the agent runtime.
///
/// # Errors
///
/// Returns [`DoctorReportError`] when the bounded blocking host-check task fails.
///
/// `supervision` is the worker supervision the daemon runs with, or `None`
/// when it has none. `catalog_trust` is what the daemon loaded to authorize
/// official packages. `overlays` is the state of the overlays the daemon was
/// started with.
#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is a distinct piece of daemon state the report reads."
)]
pub(crate) async fn report(
    paths: &Paths,
    governance: &HostGovernanceService,
    supervision: Option<ActiveSupervision>,
    catalog_trust: &CatalogTrust,
    profiles: &crate::agent::ProfileRegistry,
    base_env: pohunek_worker_protocol::BaseEnv,
    overlays: OverlayChecks<'_>,
    include_profile_homes: bool,
    rejected_lines: RejectedLinesState,
) -> Result<DoctorReport, DoctorReportError> {
    let paths = paths.clone();
    let listener_checks = hostcheck::overlay_checks::listener_checks(overlays);
    let overlay_diagnostics = overlays.diagnostics.to_vec();
    let overlays_from_file = overlays.from_file;
    let mode = supervision_mode(supervision.as_ref());
    let worker = supervision.map(|active| WorkerCandidate {
        path: active.worker_executable,
        source: WorkerSource::Supervision,
    });
    let mut checks = tokio::task::spawn_blocking(move || {
        let mut checks = standard_checks(
            &paths,
            worker.as_ref(),
            mode,
            OverlayChecks {
                diagnostics: &overlay_diagnostics,
                from_file: overlays_from_file,
            },
        );
        let readiness = worker_readiness_check(worker.as_ref());
        if let Some(existing) = checks
            .iter_mut()
            .find(|existing| existing.name == hostcheck::WORKER_EXECUTABLE_CHECK)
        {
            *existing = readiness;
        } else {
            checks.push(readiness);
        }
        checks
    })
    .await
    .map_err(|_error| DoctorReportError)?;
    checks.extend(listener_checks);
    let profiles = profiles.clone();
    checks.extend(
        tokio::task::spawn_blocking(move || {
            profile_checks(&profiles, &base_env, include_profile_homes)
        })
            .await
            .unwrap_or_else(|_| vec![check("agent_profiles", DoctorStatus::Fail, "Configured agent profiles could not be inspected; check daemon logs and profile ownership.")]),
    );
    checks.extend(governance_checks(governance.diagnose().await));
    checks.push(catalog_trust_check(catalog_trust));
    checks.push(rejected_lines_check(rejected_lines));
    Ok(bounded_report(checks))
}

/// Space held for the response envelope around the JSON doctor report.
const DOCTOR_RESPONSE_HEADROOM_BYTES: usize = 1024;
const MAX_DOCTOR_REPORT_BYTES: usize = MAX_CONTROL_LINE_BYTES - DOCTOR_RESPONSE_HEADROOM_BYTES;

/// Keep the report inside the control-line limit and make omitted findings explicit.
fn bounded_report(checks: Vec<DoctorCheck>) -> DoctorReport {
    let report = DoctorReport::from_checks(checks.clone());
    if serde_json::to_vec(&report)
        .is_ok_and(|serialized| serialized.len() <= MAX_DOCTOR_REPORT_BYTES)
    {
        return report;
    }

    let truncated = DoctorCheck::new(
        "doctor_checks_truncated",
        DoctorStatus::Fail,
        "Doctor findings exceeded the response-size limit; some checks were omitted.",
    );
    let truncated_bytes = serde_json::to_vec(&truncated).unwrap_or_default();
    let template_bytes =
        serde_json::to_vec(&DoctorReport::from_checks(vec![truncated.clone()])).unwrap_or_default();
    let report_overhead = template_bytes.len().saturating_sub(truncated_bytes.len());
    let mut retained = Vec::new();
    let mut serialized_len = report_overhead.saturating_add(truncated_bytes.len());
    for check in checks {
        let check_bytes = serde_json::to_vec(&check).unwrap_or_default();
        let next_len = serialized_len
            .saturating_add(check_bytes.len())
            .saturating_add(1);
        if next_len > MAX_DOCTOR_REPORT_BYTES {
            break;
        }
        retained.push(check);
        serialized_len = next_len;
    }
    retained.push(truncated);
    DoctorReport::from_checks(retained)
}

/// The launchd-relevant supervision mode: native jobs, direct children, or
/// unknown when the daemon has no worker supervision.
fn supervision_mode(active: Option<&ActiveSupervision>) -> Supervision {
    match active {
        Some(active) if active.native => Supervision::Native,
        Some(_) => Supervision::Subprocess,
        None => Supervision::Unknown,
    }
}

/// Check configured profile resolution and the readiness of optional providers.
fn profile_checks(
    profiles: &crate::agent::ProfileRegistry,
    base_env: &pohunek_worker_protocol::BaseEnv,
    include_profile_homes: bool,
) -> Vec<DoctorCheck> {
    let mut checks = embedded_manifest_checks();
    checks.extend(manifest_override_checks(profiles));
    let probe_started = Instant::now();
    let configured = match profiles.doctor_profiles() {
        Ok(configured) => configured,
        Err(detail) => {
            checks.push(check("agent_profiles", DoctorStatus::Fail, detail));
            checks.extend(integration_checks(profiles, base_env, false, &[]));
            return checks;
        }
    };
    configured_checks(
        profiles,
        base_env,
        include_profile_homes,
        configured,
        checks,
        probe_started,
    )
}

fn configured_checks(
    profiles: &crate::agent::ProfileRegistry,
    base_env: &pohunek_worker_protocol::BaseEnv,
    include_profile_homes: bool,
    configured: Vec<(
        String,
        Result<crate::agent::ResolvedAgent, crate::agent::DoctorProfileError>,
    )>,
    mut checks: Vec<DoctorCheck>,
    probe_started: Instant,
) -> Vec<DoctorCheck> {
    let configured_names: std::collections::HashSet<String> =
        configured.iter().map(|(name, _)| name.clone()).collect();
    let hermes_profiles: Vec<String> = configured
        .iter()
        .filter_map(|(name, result)| match result {
            Ok(agent) if agent.base.as_str() == RuntimeId::HERMES => Some(name.clone()),
            _ => None,
        })
        .collect();
    checks.extend(configured_profile_checks(
        profiles,
        base_env,
        configured,
        probe_started,
    ));
    checks.extend(optional_runtime_checks(
        profiles,
        base_env,
        &configured_names,
        probe_started,
    ));
    checks.extend(integration_checks(
        profiles,
        base_env,
        include_profile_homes,
        &hermes_profiles,
    ));
    checks
}

fn configured_profile_checks(
    profiles: &crate::agent::ProfileRegistry,
    base_env: &pohunek_worker_protocol::BaseEnv,
    configured: Vec<(
        String,
        Result<crate::agent::ResolvedAgent, crate::agent::DoctorProfileError>,
    )>,
    probe_started: Instant,
) -> Vec<DoctorCheck> {
    configured
        .into_iter()
        .map(|(name, result)| {
            let (result, readiness_lost) = match result {
                Ok(agent) => (
                    profile_runtime_check(profiles, &agent, base_env, probe_started),
                    agent.override_lacks_screen_readiness(),
                ),
                Err(crate::agent::DoctorProfileError::RuntimeUnavailable) => {
                    (Err(RuntimeFailure::PinnedRuntime), false)
                }
                Err(crate::agent::DoctorProfileError::Invalid) => {
                    (Err(RuntimeFailure::Profile), false)
                }
            };
            let (status, detail) = match result {
                Ok(()) if readiness_lost => (
                    DoctorStatus::Warn,
                    Cow::Borrowed(PROFILE_WITHOUT_SCREEN_READINESS),
                ),
                result => configured_profile_result(result),
            };
            DoctorCheck::new(
                format!("agent_profile_{}", safe_profile_name(&name)),
                status,
                detail,
            )
        })
        .collect()
}

/// Doctor detail of a profile whose override manifest cannot report the screen
/// readiness its base runtime declares (`input.ready = "screen_idle"`).
const PROFILE_WITHOUT_SCREEN_READINESS: &str =
    pohunek_service_config::preflight::PROFILE_WITHOUT_SCREEN_READINESS_DETAIL;

fn configured_profile_result(
    result: Result<(), RuntimeFailure>,
) -> (DoctorStatus, Cow<'static, str>) {
    let (status, detail) = match result {
        Ok(()) => (
            DoctorStatus::Ok,
            "Configured profile and runtime are ready.",
        ),
        Err(RuntimeFailure::OptionalMissing) => (
            DoctorStatus::Fail,
            "The configured profile runtime is unavailable; repair the profile or install its runtime.",
        ),
        Err(RuntimeFailure::PinnedRuntime) => (
            DoctorStatus::Fail,
            "The pinned runtime is unavailable; install or select its pinned package again.",
        ),
        Err(RuntimeFailure::Profile) => (
            DoctorStatus::Fail,
            "The profile is malformed or cannot be resolved securely; repair its file or permissions.",
        ),
        Err(RuntimeFailure::Executable) => (
            DoctorStatus::Fail,
            "The configured executable is missing or not trusted; install it or repair the profile.",
        ),
        Err(RuntimeFailure::Refused(refusal)) => {
            return (
                DoctorStatus::Fail,
                Cow::Owned(version_refusal_detail("The configured executable", refusal)),
            );
        }
        Err(RuntimeFailure::Version) => (
            DoctorStatus::Fail,
            "The configured executable's version probe failed; repair the profile or runtime installation. If the configured program is a wrapper, it must answer the runtime package's version-probe arguments without the profile's fixed arguments.",
        ),
        Err(RuntimeFailure::ProbeBudget) => (
            DoctorStatus::Fail,
            "The configured runtime version was not checked because the doctor probe budget was exhausted.",
        ),
    };
    (status, Cow::Borrowed(detail))
}

/// Doctor detail of a probe whose output names no supported release: the
/// parsed release and the declared range, or that the output did not parse.
/// Raw probe output never reaches the detail.
fn version_refusal_detail(subject: &str, refusal: VersionRefusal) -> String {
    match refusal {
        VersionRefusal::OutOfRange {
            version,
            min,
            below,
        } => format!(
            "{subject} reports version {version}, outside its runtime's supported range [{min}, {below}); install a release in that range or update the runtime package."
        ),
        VersionRefusal::Unparseable => format!(
            "{subject}'s version output does not match its runtime's declared format; the configured program must print its release in the format its runtime package declares. If the configured program is a wrapper, it must answer the runtime package's version-probe arguments without the profile's fixed arguments."
        ),
    }
}

fn optional_runtime_checks(
    profiles: &crate::agent::ProfileRegistry,
    base_env: &pohunek_worker_protocol::BaseEnv,
    configured_names: &std::collections::HashSet<String>,
    probe_started: Instant,
) -> Vec<DoctorCheck> {
    crate::agent::host::RESERVED_RUNTIME_IDS
        .iter()
        .copied()
        .filter(|name| *name != protocol::RuntimeId::SHELL && !configured_names.contains(*name))
        .map(|name| {
            let result = profiles
                .resolve_agent(name)
                .map_or(Err(RuntimeFailure::OptionalMissing), |agent| {
                    profile_runtime_check(profiles, &agent, base_env, probe_started)
                });
            let (status, detail) = optional_runtime_result(result);
            DoctorCheck::new(format!("agent_runtime_{name}"), status, detail)
        })
        .collect()
}

fn optional_runtime_result(
    result: Result<(), RuntimeFailure>,
) -> (DoctorStatus, Cow<'static, str>) {
    let (status, detail) = match result {
        Ok(()) => (DoctorStatus::Ok, "Optional provider runtime is ready."),
        Err(RuntimeFailure::Executable) => (
            DoctorStatus::Warn,
            "Optional provider executable is missing; install it only if you use this agent.",
        ),
        Err(RuntimeFailure::Refused(refusal)) => {
            return (
                DoctorStatus::Fail,
                Cow::Owned(version_refusal_detail("The optional provider", refusal)),
            );
        }
        Err(RuntimeFailure::Version) => (
            DoctorStatus::Fail,
            "Optional provider's version probe failed; repair or reinstall the provider. If the configured program is a wrapper, it must answer the runtime package's version-probe arguments without the profile's fixed arguments.",
        ),
        Err(RuntimeFailure::PinnedRuntime) => (
            DoctorStatus::Fail,
            "The pinned runtime is unavailable; install or select its pinned package again.",
        ),
        Err(RuntimeFailure::Profile) => (
            DoctorStatus::Fail,
            "The profile is malformed or cannot be resolved securely; repair its file or permissions.",
        ),
        Err(RuntimeFailure::OptionalMissing) => (
            DoctorStatus::Warn,
            "Optional provider is not installed; install it only if you use this agent.",
        ),
        Err(RuntimeFailure::ProbeBudget) => (
            DoctorStatus::Warn,
            "Optional provider version was not checked because the doctor probe budget was exhausted.",
        ),
    };
    (status, Cow::Borrowed(detail))
}

/// Add redacted hook-integration readiness using the launch-equivalent homes.
fn integration_checks(
    profiles: &crate::agent::ProfileRegistry,
    base_env: &pohunek_worker_protocol::BaseEnv,
    include_profile_homes: bool,
    hermes_profiles: &[String],
) -> Vec<DoctorCheck> {
    let params = IntegrationDoctorParams {
        agent: None,
        profile: None,
        all_profiles: include_profile_homes,
    };
    let homes = crate::integration::ConfigHomes::new(profiles.clone(), base_env.clone());
    let mut checks = match crate::integration::doctor_in(&homes, params) {
        Ok(report) => report
            .agents
            .iter()
            .map(integration_agent_check)
            .collect(),
        Err(_) => vec![check(
            "agent_integrations",
            DoctorStatus::Fail,
            "Provider integrations could not be safely inspected; check daemon logs and configuration ownership.",
        )],
    };
    if include_profile_homes {
        checks.extend(hermes_profiles.iter().map(|name| {
            DoctorCheck::new(
                format!("agent_integration_hermes_profile_{}", safe_profile_name(name)),
                DoctorStatus::Warn,
                "Hermes hook integration is unverified; this configured profile uses CLI-managed hooks, and the daemon has no inspection policy for its config home. Review its hook setup with Hermes's supported local tooling.",
            )
        }));
    }
    checks
}

/// Convert one typed integration diagnosis without exposing inspected paths or prose.
fn integration_agent_check(diagnosis: &protocol::IntegrationAgentDoctor) -> DoctorCheck {
    let runtime = diagnosis.agent.as_wire();
    let selector = diagnosis
        .home
        .as_ref()
        .and_then(|home| home.selector.as_ref());
    let profile = match selector {
        Some(protocol::IntegrationSelector::Profile { name }) => Some(name.as_str()),
        Some(protocol::IntegrationSelector::Default) | None => None,
    };
    let target = profile
        .map(|name| format!("_profile_{}", safe_profile_name(name)))
        .unwrap_or_default();
    let name = format!("agent_integration_{runtime}{target}");
    let recovery = profile.map_or_else(
        || format!("pohunek integration install --agent {runtime}"),
        |profile| format!("pohunek integration install --agent {runtime} --profile {profile}"),
    );
    let shared_profiles = diagnosis
        .home
        .as_ref()
        .map_or(0, |home| home.profiles.len().saturating_sub(1));
    let repair_required = diagnosis.status.as_ref().is_some_and(|status| {
        status.recovery == protocol::IntegrationRecovery::RepairConfiguration
    });
    let ambiguous_owner = diagnosis
        .findings
        .iter()
        .any(crate::integration::is_ambiguous_owner);
    let has_security_or_malformed_finding = diagnosis.findings.iter().any(|finding| {
        !crate::integration::is_ambiguous_owner(finding)
            && matches!(
                finding.code,
                IntegrationFindingCode::ConfigRootInvalid
                    | IntegrationFindingCode::ProviderConfigInvalid
                    | IntegrationFindingCode::HookSocketPathInvalid
                    | IntegrationFindingCode::UnsafeInstallerLock
                    | IntegrationFindingCode::DisplacedOriginalLeftBehind
                    | IntegrationFindingCode::QuarantineScanIncomplete
            )
            || (finding.code == IntegrationFindingCode::AssetUnsafe && repair_required)
    });
    let (status, detail) = match &diagnosis.status {
        None => (
            DoctorStatus::Warn,
            "Integration inspection is busy; retry doctor after the current install or uninstall finishes.".to_owned(),
        ),
        Some(_) if ambiguous_owner && !has_security_or_malformed_finding => (
            DoctorStatus::Warn,
            format!("The config home is reached by several runtimes through the same integration handler, so it is attributed to none of them; name the runtime with `--agent {runtime}` (for example `{recovery}`) or give the runtimes different config homes."),
        ),
        Some(_) if has_security_or_malformed_finding => (
            DoctorStatus::Fail,
            format!("Provider integration state is malformed, unsafe, or incomplete; repair it before running `{recovery}`."),
        ),
        Some(status) if status.state == IntegrationInstallState::Current && diagnosis.ok => (
            DoctorStatus::Ok,
            "Managed integration is current.".to_owned(),
        ),
        Some(status) if status.state == IntegrationInstallState::NotInstalled => (
            DoctorStatus::Warn,
            format!("Managed integration is absent; install it if hooks are needed with `{recovery}`."),
        ),
        Some(status) => {
            let state = if status.state == IntegrationInstallState::Outdated {
                "outdated"
            } else {
                "unhealthy"
            };
            let shared = if shared_profiles == 0 {
                String::new()
            } else {
                format!(" This home is shared with {shared_profiles} other configured profile(s).")
            };
            (
                DoctorStatus::Warn,
                format!("Managed integration is {state}; repair provider configuration if needed, then run `{recovery}`.{shared}"),
            )
        }
    };
    DoctorCheck::new(name, status, &detail)
}

/// Validate the canonical embedded manifest sources without invoking their panic-on-error loaders.
fn embedded_manifest_checks() -> Vec<DoctorCheck> {
    crate::detect::embedded_manifest_sources()
        .into_iter()
        .map(|(runtime, source)| {
            let (status, detail) = match crate::detect::Manifest::parse_str(source) {
                Ok(_) => (DoctorStatus::Ok, "Embedded detection manifest is valid."),
                Err(_) => (
                    DoctorStatus::Fail,
                    "Embedded detection manifest is invalid; repair the daemon build input.",
                ),
            };
            DoctorCheck::new(format!("agent_manifest_embedded_{runtime}"), status, detail)
        })
        .collect()
}

/// Report host overrides validated by the profile layer's launch-equivalent resolver.
fn manifest_override_checks(profiles: &crate::agent::ProfileRegistry) -> Vec<DoctorCheck> {
    match profiles.doctor_manifest_overrides() {
        Ok(overrides) => overrides
            .into_iter()
            .map(|(name, referenced, result)| {
                let (status, detail) = match result {
                    Ok(true) => (
                        DoctorStatus::Ok,
                        "Manifest override is valid and intentionally disables detection.",
                    ),
                    Ok(false) if referenced => (
                        DoctorStatus::Ok,
                        "Manifest override is valid and takes precedence for its configured profile.",
                    ),
                    Ok(false) => (
                        DoctorStatus::Ok,
                        "Unreferenced manifest override is valid.",
                    ),
                    Err(crate::agent::DoctorManifestError::Invalid) if referenced => (
                        DoctorStatus::Fail,
                        "A configured profile references a missing or invalid manifest override; repair the override.",
                    ),
                    Err(crate::agent::DoctorManifestError::Invalid) => (
                        DoctorStatus::Warn,
                        "An unreferenced manifest override is invalid; repair or remove it.",
                    ),
                };
                DoctorCheck::new(
                    format!("agent_manifest_override_{}", safe_profile_name(&name)),
                    status,
                    detail,
                )
            })
            .collect(),
        Err(detail) => vec![check(
            "agent_manifest_overrides",
            DoctorStatus::Fail,
            detail,
        )],
    }
}

#[derive(Clone, Copy)]
enum RuntimeFailure {
    OptionalMissing,
    Profile,
    PinnedRuntime,
    Executable,
    /// The probe ran and its output names no supported release.
    Refused(VersionRefusal),
    /// The probe failed or its parser is not compiled in.
    Version,
    ProbeBudget,
}

/// Validate a profile using the same package and provider-version gates as launch.
fn profile_runtime_check(
    profiles: &crate::agent::ProfileRegistry,
    agent: &crate::agent::ResolvedAgent,
    base_env: &pohunek_worker_protocol::BaseEnv,
    probe_started: Instant,
) -> Result<(), RuntimeFailure> {
    profiles
        .runtimes()
        .verify_launchable(&agent.definition)
        .map_err(|_error| RuntimeFailure::PinnedRuntime)?;
    let profile_env = agent
        .profile
        .as_ref()
        .map_or(&[][..], |profile| profile.env.as_slice());
    let launch_path =
        crate::runtime::environment::effective_variable(base_env, profile_env, "PATH");
    if crate::agent::which_executable(agent.program()).is_none() {
        return Err(RuntimeFailure::Executable);
    }
    let probe_timeout = if agent.definition.version_probe_parser().is_some() {
        hostcheck::AGENT_RUNTIME_PROBE_BUDGET.saturating_sub(probe_started.elapsed())
    } else {
        Duration::ZERO
    };
    if agent.definition.version_probe_parser().is_some() && probe_timeout.is_zero() {
        return Err(RuntimeFailure::ProbeBudget);
    }
    crate::agent::host::validate_launch_runtime_with_timeout_detailed(
        &agent.definition,
        agent.spawn(),
        launch_path.as_deref(),
        probe_timeout,
    )
    .map_err(|error| match error {
        crate::capabilities::LaunchValidationFailure::Missing => RuntimeFailure::Executable,
        crate::capabilities::LaunchValidationFailure::ProbeTimedOut => RuntimeFailure::ProbeBudget,
        crate::capabilities::LaunchValidationFailure::Refused(refusal) => {
            RuntimeFailure::Refused(refusal)
        }
        crate::capabilities::LaunchValidationFailure::Unsupported => RuntimeFailure::Version,
    })?;
    Ok(())
}

/// Restrict dynamic check-name components to profile-name characters.
fn safe_profile_name(name: &str) -> String {
    if crate::project::config::validate_name("agent", name).is_ok() {
        name.to_owned()
    } else {
        "invalid_name".to_owned()
    }
}

fn standard_checks(
    paths: &Paths,
    worker: Option<&WorkerCandidate>,
    supervision: Supervision,
    overlays: OverlayChecks<'_>,
) -> Vec<DoctorCheck> {
    let home_dir = std::env::var_os(pohunek_paths::HOME).map(PathBuf::from);
    hostcheck::standard_checks(StandardCheckInputs {
        socket_dir: &paths.runtime_dir,
        state_dir: &paths.data_dir,
        log_dir: &paths.log_dir,
        config_dir: &paths.config_dir,
        home_dir: home_dir.as_deref(),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
        worker,
        access_dirs: &[],
        supervision,
        overlays,
    })
}

/// The daemon's active worker executable and protocol readiness.
fn worker_readiness_check(worker: Option<&WorkerCandidate>) -> DoctorCheck {
    let Some(worker) = worker else {
        return check(
            hostcheck::WORKER_EXECUTABLE_CHECK,
            DoctorStatus::Fail,
            "The daemon has no active session worker; configure worker supervision before starting sessions.",
        );
    };
    if !worker.path.is_absolute() || !hostcheck::is_executable_file(&worker.path) {
        return check(
            hostcheck::WORKER_EXECUTABLE_CHECK,
            DoctorStatus::Fail,
            "The active session worker is missing or unsafe; install a trusted worker executable and restart the daemon.",
        );
    }

    match hostcheck::worker_probe::probe_worker(
        &worker.path,
        hostcheck::WORKER_PROBE_DEADLINE,
        crate::runtime::lifecycle::WORKER_JOURNAL_READABLE_SCHEMAS,
        pohunek_worker_protocol::SUPPORTED_RANGE,
    ) {
        Ok(_) => check(
            hostcheck::WORKER_EXECUTABLE_CHECK,
            DoctorStatus::Ok,
            "The active session worker reports a readable journal schema and overlapping worker protocol.",
        ),
        Err(hostcheck::worker_probe::WorkerProbeError::Unsupported) => check(
            hostcheck::WORKER_EXECUTABLE_CHECK,
            DoctorStatus::Warn,
            "The active session worker does not report capabilities; compatibility is unverified.",
        ),
        Err(hostcheck::worker_probe::WorkerProbeError::TimedOut) => check(
            hostcheck::WORKER_EXECUTABLE_CHECK,
            DoctorStatus::Fail,
            "The active session worker did not answer the bounded capability probe; repair or reinstall it.",
        ),
        Err(_) => check(
            hostcheck::WORKER_EXECUTABLE_CHECK,
            DoctorStatus::Fail,
            "The active session worker is malformed or incompatible; repair or reinstall the worker binary.",
        ),
    }
}

/// Name of the check that reports the catalog trust anchor.
const CATALOG_TRUST_CHECK: &str = "catalog_trust_anchor";

/// The check for the anchor that authorizes official packages: an absent
/// anchor is a warning (official packages cannot be installed, the daemon is
/// otherwise unaffected) and an untrusted one a failure.
fn catalog_trust_check(trust: &CatalogTrust) -> DoctorCheck {
    match trust {
        CatalogTrust::Loaded(_) => check(
            CATALOG_TRUST_CHECK,
            DoctorStatus::Ok,
            "The catalog trust anchor is loaded; official packages can be authorized.",
        ),
        CatalogTrust::Absent => check(
            CATALOG_TRUST_CHECK,
            DoctorStatus::Warn,
            "This installation ships no catalog trust anchor; official packages cannot be installed.",
        ),
        CatalogTrust::Invalid(fault) => check(
            CATALOG_TRUST_CHECK,
            DoctorStatus::Fail,
            &invalid_anchor_detail(*fault),
        ),
    }
}

fn invalid_anchor_detail(fault: AnchorFault) -> String {
    format!("{fault}; official packages are refused until the anchor file is fixed and the daemon restarts.")
}

/// The check that reports the metadata-store lines no loader accepts. It
/// carries counts and the command that lists them, never a line's text.
fn rejected_lines_check(state: RejectedLinesState) -> DoctorCheck {
    match state {
        RejectedLinesState::Counted { total: 0, .. } => check(
            METADATA_STORE_REJECTED_LINES_CHECK,
            DoctorStatus::Ok,
            "The metadata store holds no line this build cannot load.",
        ),
        RejectedLinesState::Counted { total, may_pin } => check(
            METADATA_STORE_REJECTED_LINES_CHECK,
            DoctorStatus::Warn,
            &format!(
                "The metadata store keeps {total} line(s) this build cannot load, {may_pin} of them may pin a runtime package, so package removal and selection fail closed while they stay. Run `pohunek store rejected list`, then `pohunek store rejected set-aside <content-hash>`."
            ),
        ),
        RejectedLinesState::Unreadable => check(
            METADATA_STORE_REJECTED_LINES_CHECK,
            DoctorStatus::Fail,
            "The metadata store cannot be read; check the daemon log.",
        ),
    }
}

fn governance_checks(diagnostic: HostGovernanceDiagnostic) -> [DoctorCheck; 6] {
    match diagnostic {
        HostGovernanceDiagnostic::Available(status) => {
            let (consistency, quarantine) = quarantine_checks(status.quarantine());
            [
                check(
                    "host_governance_durability",
                    DoctorStatus::Ok,
                    "Stable host governance is durably loaded.",
                ),
                check(
                    "host_identity_stable",
                    DoctorStatus::Ok,
                    "Stable host identity matches the retained durable state.",
                ),
                check(
                    "host_state_private_storage",
                    DoctorStatus::Ok,
                    "Host-state storage remains owner-private and link-safe.",
                ),
                consistency,
                check(
                    "host_approval_key",
                    DoctorStatus::Ok,
                    "Approval signing material remains separate and verifies locally.",
                ),
                quarantine,
            ]
        }
        HostGovernanceDiagnostic::ReloadRequired => unavailable_checks(
            DoctorStatus::Fail,
            "Durable state requires an explicit daemon reload before governance use.",
        ),
        HostGovernanceDiagnostic::IntegrityFailed => unavailable_checks(
            DoctorStatus::Fail,
            "Durable host governance could not be safely revalidated; inspect owner-private storage.",
        ),
        HostGovernanceDiagnostic::Unavailable => unavailable_checks(
            DoctorStatus::Fail,
            "Host governance is unavailable; restart the daemon and retry inspection.",
        ),
    }
}

fn unavailable_checks(status: DoctorStatus, detail: &str) -> [DoctorCheck; 6] {
    [
        check("host_governance_durability", status, detail),
        check("host_identity_stable", status, detail),
        check("host_state_private_storage", status, detail),
        check("host_governance_consistency", status, detail),
        check("host_approval_key", status, detail),
        check("host_governance_quarantine", status, detail),
    ]
}

fn quarantine_checks(reason: Option<QuarantineReason>) -> (DoctorCheck, DoctorCheck) {
    match reason {
        None => (
            check(
                "host_governance_consistency",
                DoctorStatus::Ok,
                "Governance records are mutually consistent.",
            ),
            check(
                "host_governance_quarantine",
                DoctorStatus::Ok,
                "Host governance is not quarantined.",
            ),
        ),
        Some(QuarantineReason::ProjectionConflict) => quarantine_warning(
            "Relay projection conflicts with durable governance; reconcile the registered relay before use.",
        ),
        Some(QuarantineReason::EnrollmentConflict) => quarantine_warning(
            "Enrollment coordinates conflict; reconcile the registered relay before use.",
        ),
        Some(QuarantineReason::Unknown) => (
            check(
                "host_governance_consistency",
                DoctorStatus::Fail,
                "Host governance is quarantined for a reason this build does not know; upgrade this daemon or reconcile the registered relay before use.",
            ),
            check(
                "host_governance_quarantine",
                DoctorStatus::Fail,
                "Host governance is quarantined for a reason this build does not know.",
            ),
        ),
        Some(QuarantineReason::HostIdentityClone) => (
            check(
                "host_governance_consistency",
                DoctorStatus::Fail,
                "A stable host identity clone was detected; stop the conflicting host before use.",
            ),
            check(
                "host_governance_quarantine",
                DoctorStatus::Fail,
                "Host governance is quarantined for a stable host identity clone.",
            ),
        ),
    }
}

fn quarantine_warning(detail: &'static str) -> (DoctorCheck, DoctorCheck) {
    (
        check("host_governance_consistency", DoctorStatus::Warn, detail),
        check("host_governance_quarantine", DoctorStatus::Warn, detail),
    )
}

fn check(name: &'static str, status: DoctorStatus, detail: &str) -> DoctorCheck {
    DoctorCheck::new(name, status, detail)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};

    use super::*;

    fn paths_at(root: &Path) -> Paths {
        Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            lock: root.join("runtime").join("daemon.lock"),
            log_dir: root.join("logs"),
            state_dir: root.join("state"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            config_home: root.join("config"),
            config_dir: root.join("config").join("pohunek"),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        // The directory lives until the test's thread ends; the path below it
        // does not exist yet.
        crate::test_support::thread_scoped_dir(&format!("pohunek-daemon-doctor-{tag}-"))
            .join("root")
    }

    #[test]
    fn the_supervision_mode_follows_the_active_supervision() {
        let active = |native| ActiveSupervision {
            worker_executable: PathBuf::from("/opt/pohunek-sessiond"),
            native,
        };

        assert_eq!(supervision_mode(Some(&active(true))), Supervision::Native);
        assert_eq!(
            supervision_mode(Some(&active(false))),
            Supervision::Subprocess
        );
        assert_eq!(supervision_mode(None), Supervision::Unknown);
    }

    #[tokio::test]
    async fn report_contains_writable_daemon_paths_and_governance_checks() {
        let root = temp_dir("report");
        let paths = paths_at(&root);

        let service = crate::governance::HostGovernanceService::open_test();
        let report = report(
            &paths,
            &service,
            None,
            &CatalogTrust::Absent,
            &crate::agent::ProfileRegistry::default(),
            pohunek_worker_protocol::BaseEnv::default(),
            OverlayChecks::NONE,
            false,
            RejectedLinesState::Counted {
                total: 0,
                may_pin: 0,
            },
        )
        .await
        .expect("doctor report");

        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "socket_dir_writable" && check.status == DoctorStatus::Ok));
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "state_dir_writable" && check.status == DoctorStatus::Ok));
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "log_dir_writable" && check.status == DoctorStatus::Ok));
        assert_eq!(
            report
                .checks
                .iter()
                .filter(|check| check.name.starts_with("host_"))
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>(),
            [
                "host_governance_durability",
                "host_identity_stable",
                "host_state_private_storage",
                "host_governance_consistency",
                "host_approval_key",
                "host_governance_quarantine",
            ]
        );
    }

    #[test]
    fn the_catalog_trust_check_reports_each_state_with_its_severity() {
        let loaded = package::parse_anchor(&{
            let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
            let root = package::RootKey::new(key.verifying_key().to_bytes(), 1, 2).expect("root");
            package::AnchorFile::new(vec![root], Vec::new())
                .expect("anchor")
                .to_bytes()
                .expect("bytes")
        })
        .expect("parse")
        .into_parts();
        let anchor = crate::session::HostTrustAnchor::new(loaded.0, loaded.1).expect("anchor");
        let check = catalog_trust_check(&CatalogTrust::Loaded(anchor));
        assert_eq!(
            (check.name.as_str(), check.status),
            (CATALOG_TRUST_CHECK, DoctorStatus::Ok)
        );

        let absent = catalog_trust_check(&CatalogTrust::Absent);
        assert_eq!(absent.status, DoctorStatus::Warn);
        assert!(absent.detail.contains("no catalog trust anchor"));

        let invalid = catalog_trust_check(&CatalogTrust::Invalid(AnchorFault::UnsafeFile));
        assert_eq!(invalid.status, DoctorStatus::Fail);
        assert!(invalid.detail.contains("not safe"), "{}", invalid.detail);
        assert!(!invalid.detail.contains('/'), "no path in the detail");
    }

    #[tokio::test]
    async fn an_untrusted_anchor_makes_the_report_fail() {
        let root = temp_dir("untrusted-anchor");
        let paths = paths_at(&root);
        let service = crate::governance::HostGovernanceService::open_test();
        let report = report(
            &paths,
            &service,
            None,
            &CatalogTrust::Invalid(AnchorFault::TooLarge),
            &crate::agent::ProfileRegistry::default(),
            pohunek_worker_protocol::BaseEnv::default(),
            OverlayChecks::NONE,
            false,
            RejectedLinesState::Counted {
                total: 0,
                may_pin: 0,
            },
        )
        .await
        .expect("doctor report");
        let check = report
            .checks
            .iter()
            .find(|check| check.name == CATALOG_TRUST_CHECK)
            .expect("the trust check is reported");
        assert_eq!(check.status, DoctorStatus::Fail);
        assert_eq!(report.overall, DoctorStatus::Fail);
    }

    #[test]
    fn available_governance_matrix_is_ok_for_never_enrolled_principal_and_team() {
        for (name, owner) in [
            ("never enrolled", None),
            ("principal", Some(principal_owner())),
            ("team", Some(team_owner())),
        ] {
            assert_governance_case(
                name,
                HostGovernanceDiagnostic::Available(available_status(owner)),
                [DoctorStatus::Ok; 6],
                DoctorStatus::Ok,
            );
        }
    }

    #[test]
    fn quarantined_governance_matrix_preserves_warning_and_failure_severities() {
        for (name, reason, statuses, overall) in [
            (
                "projection conflict",
                QuarantineReason::ProjectionConflict,
                warning_statuses(),
                DoctorStatus::Warn,
            ),
            (
                "enrollment conflict",
                QuarantineReason::EnrollmentConflict,
                warning_statuses(),
                DoctorStatus::Warn,
            ),
            (
                "identity clone",
                QuarantineReason::HostIdentityClone,
                identity_clone_statuses(),
                DoctorStatus::Fail,
            ),
        ] {
            assert_governance_case(
                name,
                HostGovernanceDiagnostic::Available(quarantined_status(reason)),
                statuses,
                overall,
            );
        }
    }

    #[test]
    fn unavailable_governance_matrix_fails_every_check() {
        for (name, diagnostic) in [
            ("reload required", HostGovernanceDiagnostic::ReloadRequired),
            (
                "integrity failed",
                HostGovernanceDiagnostic::IntegrityFailed,
            ),
            ("unavailable", HostGovernanceDiagnostic::Unavailable),
        ] {
            assert_governance_case(
                name,
                diagnostic,
                [DoctorStatus::Fail; 6],
                DoctorStatus::Fail,
            );
        }
    }

    fn assert_governance_case(
        name: &str,
        diagnostic: HostGovernanceDiagnostic,
        expected_statuses: [DoctorStatus; 6],
        expected_overall: DoctorStatus,
    ) {
        let checks = governance_checks(diagnostic);
        assert_eq!(
            checks.clone().map(|check| check.name),
            governance_check_names(),
            "{name} check order"
        );
        assert_eq!(
            checks.clone().map(|check| check.status),
            expected_statuses,
            "{name} statuses"
        );
        assert_eq!(
            DoctorReport::from_checks(Vec::from(checks)).overall,
            expected_overall,
            "{name} overall severity"
        );
    }

    fn governance_check_names() -> [&'static str; 6] {
        [
            "host_governance_durability",
            "host_identity_stable",
            "host_state_private_storage",
            "host_governance_consistency",
            "host_approval_key",
            "host_governance_quarantine",
        ]
    }

    fn warning_statuses() -> [DoctorStatus; 6] {
        [
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Warn,
            DoctorStatus::Ok,
            DoctorStatus::Warn,
        ]
    }

    fn identity_clone_statuses() -> [DoctorStatus; 6] {
        [
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Fail,
            DoctorStatus::Ok,
            DoctorStatus::Fail,
        ]
    }

    fn principal_owner() -> protocol::HostOwner {
        protocol::HostOwner::Principal(
            protocol::PrincipalId::parse(&identifier("principal_", 3)).expect("principal"),
        )
    }

    fn team_owner() -> protocol::HostOwner {
        protocol::HostOwner::Team(protocol::TeamId::parse(&identifier("team_", 4)).expect("team"))
    }

    fn available_status(owner: Option<protocol::HostOwner>) -> protocol::HostGovernanceStatus {
        let enrolled = owner.is_some();
        protocol::HostGovernanceStatus::new(
            protocol::HostId::parse(&identifier("host_", 1)).expect("host"),
            enrolled.then(|| {
                protocol::EnrollmentInfo::new(
                    protocol::RelayId::parse(&identifier("relay_", 2)).expect("relay"),
                    protocol::EnrollmentStatus::Active,
                    protocol::EnrollmentRevision::new(1).expect("revision"),
                )
            }),
            owner,
            enrolled.then(|| protocol::OwnerRevision::new(1).expect("revision")),
            None,
            protocol::ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
        )
        .expect("valid available status")
    }

    fn quarantined_status(reason: QuarantineReason) -> protocol::HostGovernanceStatus {
        protocol::HostGovernanceStatus::new(
            protocol::HostId::parse(&identifier("host_", 1)).expect("host"),
            Some(protocol::EnrollmentInfo::new(
                protocol::RelayId::parse(&identifier("relay_", 2)).expect("relay"),
                protocol::EnrollmentStatus::Quarantined,
                protocol::EnrollmentRevision::new(1).expect("revision"),
            )),
            Some(protocol::HostOwner::Principal(
                protocol::PrincipalId::parse(&identifier("principal_", 3)).expect("principal"),
            )),
            Some(protocol::OwnerRevision::new(1).expect("revision")),
            Some(reason),
            protocol::ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
        )
        .expect("valid quarantined status")
    }

    fn identifier(prefix: &str, byte: u8) -> String {
        format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode([byte; 32]))
    }
}
